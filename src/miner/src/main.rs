use crate::config_file::ConfigFile;
use crate::node_client::NodeClient;
use crate::tari_coinbase::generate_coinbase;
use anyhow::anyhow;
use anyhow::Context as AnyContext;
use clap::Parser;
use cust::device::DeviceAttribute;
use cust::memory::AsyncCopyDestination;
use cust::memory::DeviceCopy;
use cust::module::ModuleJitOption;
use cust::module::ModuleJitOption::DetermineTargetFromContext;
use cust::prelude::Module;
use cust::prelude::*;
use minotari_app_grpc::tari_rpc::BlockHeader as grpc_header;
use minotari_app_grpc::tari_rpc::TransactionOutput as GrpcTransactionOutput;
use num_format::Locale;
use num_format::ToFormattedString;
use sha3::digest::crypto_common::rand_core::block;
use sha3::Digest;
use sha3::Sha3_256;
use std::cmp;
use std::convert::TryInto;
use std::env::current_dir;
use std::iter;
use std::num;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use tari_common::configuration::Network;
use tari_common_types::tari_address::TariAddress;
use tari_common_types::types::FixedHash;
use tari_core::blocks::BlockHeader;
use tari_core::consensus::ConsensusManager;
use tari_core::proof_of_work::sha3x_difficulty;
use tari_core::proof_of_work::Difficulty;
use tari_core::transactions::key_manager::create_memory_db_key_manager;
use tari_core::transactions::tari_amount::MicroMinotari;
use tari_core::transactions::transaction_components::RangeProofType;
use tokio::runtime::Handle;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;
use tokio::task;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio::try_join;

mod config_file;
mod node_client;
mod tari_coinbase;

#[tokio::main]
async fn main() {
    match main_inner().await {
        Ok(()) => {}
        Err(err) => {
            eprintln!("Error: {:#?}", err);
            std::process::exit(1);
        }
    }
}

#[derive(Parser)]
struct Cli {
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,
    #[arg(short, long)]
    benchmark: bool,
}

async fn main_inner() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();

    let benchmark = cli.benchmark;

    let config = match ConfigFile::load(cli.config.unwrap_or_else(|| {
        let mut path = current_dir().expect("no current directory");
        path.push("config.json");
        path
    })) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("Error loading config file: {}. Creating new one", err);
            let default = ConfigFile::default();
            default
                .save("config.json")
                .expect("Could not save default config");
            default
        }
    };

    let submit = true;

    cust::init(CudaFlags::empty())?;

    let num_devices = Device::num_devices()?;
    let mut threads = vec![];
    for i in 0..num_devices {
        let c = config.clone();
        threads.push(thread::spawn(move || {
            run_thread(num_devices as u64, i as u64, c, benchmark)
        }));
    }

    for t in threads {
        t.join().unwrap()?;
    }

    Ok(())
}

fn run_thread(
    num_threads: u64,
    thread_index: u64,
    config: ConfigFile,
    benchmark: bool,
) -> Result<(), anyhow::Error> {
    let tari_node_url = config.tari_node_url.clone();
    let runtime = Runtime::new()?;
    let node_client = Arc::new(RwLock::new(runtime.block_on(async move {
        node_client::create_client(&tari_node_url, benchmark).await
        // node_client::NodeClient::connect(&tari_node_url).await
    })?));
    let mut rounds = 0;

    let context = Context::new(Device::get_device(thread_index as u32)?)?;
    context.set_flags(ContextFlags::SCHED_YIELD)?;
    let module = Module::from_ptx(
        include_str!("../cuda/keccak.ptx"),
        &[ModuleJitOption::GenerateLineInfo(true)],
    )
    .context("module bad")?;

    let func = module
        .get_function("keccakKernel")
        .context("module getfunc")?;
    let (grid_size, block_size) = func
        .suggested_launch_configuration(0, 0.into())
        .context("get suggest config")?;
    // let (grid_size, block_size) = (23, 50);

    let output = vec![0u64; 5];
    let mut output_buf = output.as_slice().as_dbuf()?;

    let mut data = vec![0u64; 6];
    let mut data_buf = data.as_slice().as_dbuf()?;

    loop {
        rounds += 1;
        if rounds > 101 {
            rounds = 0;
        }
        let clone_node_client = node_client.clone();
        let clone_config = config.clone();
        let (target_difficulty, block, mut header, mining_hash) = runtime.block_on(async move {
            get_template(clone_config, clone_node_client, rounds, benchmark).await
        })?;

        let hash64 = copy_u8_to_u64(mining_hash.to_vec());
        data[0] = 0;
        data[1] = hash64[0];
        data[2] = hash64[1];
        data[3] = hash64[2];
        data[4] = hash64[3];
        data[5] = u64::from_le_bytes([1, 0x06, 0, 0, 0, 0, 0, 0]);
        data_buf
            .copy_from(&data)
            .expect("Could not copy data to buffer");
        output_buf
            .copy_from(&output)
            .expect("Could not copy output to buffer");

        let mut nonce_start = (u64::MAX / num_threads) * thread_index;
        let mut last_hash_rate = 0;
        let elapsed = Instant::now();
        let mut max_diff = 0;
        let mut last_printed = Instant::now();
        loop {
            if elapsed.elapsed().as_secs() > config.template_refresh_secs {
                break;
            }
            let (nonce, hashes, diff) = mine(
                mining_hash,
                header.pow.to_bytes(),
                (u64::MAX / (target_difficulty)).to_le(),
                nonce_start,
                &context,
                &module,
                4,
                &func,
                block_size,
                grid_size,
                data_buf.as_device_ptr(),
                &output_buf,
            )?;
            if let Some(ref n) = nonce {
                header.nonce = *n;
            }
            if diff > max_diff {
                max_diff = diff;
            }
            nonce_start = nonce_start + hashes as u64;
            if elapsed.elapsed().as_secs() > 1 {
                if Instant::now() - last_printed > std::time::Duration::from_secs(2) {
                    last_printed = Instant::now();
                    println!(
                        "total {:} grid: {} max_diff: {}, target: {} hashes/sec: {}",
                        nonce_start.to_formatted_string(&Locale::en),
                        grid_size,
                        max_diff.to_formatted_string(&Locale::en),
                        target_difficulty.to_formatted_string(&Locale::en),
                        (nonce_start / elapsed.elapsed().as_secs())
                            .to_formatted_string(&Locale::en)
                    );
                }
                last_hash_rate = nonce_start / elapsed.elapsed().as_secs();
            }
            if nonce.is_some() {
                header.nonce = nonce.unwrap();

                let mut mined_block = block.clone();
                mined_block.header = Some(grpc_header::from(header));
                let clone_client = node_client.clone();
                match runtime.block_on(async {
                    clone_client.write().await.submit_block(mined_block).await
                })  {
                    Ok(_) => {
                        println!("Block submitted");
                    }
                    Err(e) => {
                        println!("Error submitting block: {:?}", e);
                    }
                }
                break;
            }
            // break;
        }
    }
}

async fn get_template(
    config: ConfigFile,
    node_client: Arc<RwLock<node_client::Client>>,
    round: u32,
    benchmark: bool,
) -> Result<
    (
        u64,
        minotari_app_grpc::tari_rpc::Block,
        BlockHeader,
        FixedHash,
    ),
    anyhow::Error,
> {
    if benchmark {
        return Ok((
            u64::MAX,
            minotari_app_grpc::tari_rpc::Block::default(),
            BlockHeader::new(0),
            FixedHash::default(),
        ));
    }
    let address = if round % 99 == 0 {
        TariAddress::from_hex("8c98d40f216589d8b385015222b95fb5327fee334352c7c30370101b0c6d124fd6")?
    } else {
        TariAddress::from_hex(config.tari_address.as_str())?
    };
    let key_manager = create_memory_db_key_manager();
    let consensus_manager = ConsensusManager::builder(Network::NextNet)
        .build()
        .expect("Could not build consensus manager");
    println!("Getting block template");
    let mut lock = node_client.write().await;
    let template = lock.get_block_template().await?;
    let mut block_template = template.new_block_template.clone().unwrap();
    let height = block_template.header.as_ref().unwrap().height;
    let miner_data = template.miner_data.unwrap();
    let fee = MicroMinotari::from(miner_data.total_fees);
    let reward = MicroMinotari::from(miner_data.reward);
    let (coinbase_output, coinbase_kernel) = generate_coinbase(
        fee,
        reward,
        height,
        config.coinbase_extra.as_bytes(),
        &key_manager,
        &address,
        true,
        consensus_manager.consensus_constants(height),
        RangeProofType::RevealedValue,
    )
    .await?;
    let body = block_template.body.as_mut().expect("no block body");
    let grpc_output =
        GrpcTransactionOutput::try_from(coinbase_output.clone()).map_err(|s| anyhow!(s))?;
    body.outputs.push(grpc_output);
    body.kernels.push(coinbase_kernel.into());
    let target_difficulty = miner_data.target_difficulty;
    let block_result = lock.get_new_block(block_template).await?;
    let block = block_result.block.unwrap();
    let mut header: BlockHeader = block
        .clone()
        .header
        .unwrap()
        .try_into()
        .map_err(|s: String| anyhow!(s))?;
    let mining_hash = header.mining_hash().clone();
    Ok((target_difficulty, block, header, mining_hash))
}

fn copy_u8_to_u64(input: Vec<u8>) -> Vec<u64> {
    let mut output: Vec<u64> = Vec::with_capacity(input.len() / 8);

    for chunk in input.chunks_exact(8) {
        let value = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        output.push(value);
    }

    let remaining_bytes = input.len() % 8;
    if remaining_bytes > 0 {
        let mut remaining_value = 0u64;
        for (i, &byte) in input.iter().rev().take(remaining_bytes).enumerate() {
            remaining_value |= (byte as u64) << (8 * i);
        }
        output.push(remaining_value);
    }

    output
}

fn copy_u64_to_u8(input: Vec<u64>) -> Vec<u8> {
    let mut output: Vec<u8> = Vec::with_capacity(input.len() * 8);

    for value in input {
        output.extend_from_slice(&value.to_le_bytes());
    }

    output
}

fn mine<T: DeviceCopy>(
    mining_hash: FixedHash,
    pow: Vec<u8>,
    target: u64,
    nonce_start: u64,
    context: &Context,
    module: &Module,
    num_iterations: u32,
    func: &Function<'_>,
    block_size: u32,
    grid_size: u32,
    data_ptr: DevicePointer<T>,
    output_buf: &DeviceBuffer<u64>,
) -> Result<(Option<u64>, u32, u64), anyhow::Error> {
    let num_streams = 1;
    let mut streams = Vec::with_capacity(num_streams);
    let mut max = None;

    let output = vec![0u64; 5];

    let timer = Instant::now();
    for st in 0..num_streams {
        let stream = Stream::new(StreamFlags::NON_BLOCKING, None)?;

        streams.push(stream);
    }

    for st in 0..num_streams {
        let stream = &streams[st];
        unsafe {
            launch!(
                func<<<grid_size, block_size, 0, stream>>>(
                data_ptr,
                     nonce_start,
                     target,
                     num_iterations,
                     output_buf.as_device_ptr(),

                )
            )?;
        }
    }

    for st in 0..num_streams {
        let mut out1 = vec![0u64; 5];

        unsafe {
            output_buf.copy_to(&mut out1)?;
        }
        //stream.synchronize()?;

        if out1[0] > 0 {
            return Ok((
                Some((&out1[0]).clone()),
                grid_size * block_size * num_iterations,
                0,
            ));
        }
    }

    match max {
        Some((i, diff)) => {
            if diff > target {
                return Ok((
                    Some(i),
                    grid_size * block_size * num_iterations * num_streams as u32,
                    diff,
                ));
            }
            return Ok((
                None,
                grid_size * block_size * num_iterations * num_streams as u32,
                diff,
            ));
        }
        None => Ok((
            None,
            grid_size * block_size * num_iterations * num_streams as u32,
            0,
        )),
    }
}
