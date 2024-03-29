use minotari_app_grpc::tari_rpc::AggregateBody;
use minotari_app_grpc::tari_rpc::Block;
use minotari_app_grpc::tari_rpc::GetNewBlockResult;
use minotari_app_grpc::tari_rpc::NewBlockTemplate;
use minotari_app_grpc::tari_rpc::{
    base_node_client::BaseNodeClient, pow_algo::PowAlgos, Empty, NewBlockTemplateRequest,
    NewBlockTemplateResponse, PowAlgo,
};
use tari_core::validation::aggregate_body;

pub(crate) struct NodeClient {
    client: BaseNodeClient<tonic::transport::Channel>,
}

impl NodeClient {
    pub async fn connect(url: &str) -> Result<Self, anyhow::Error> {
        println!("Connecting to {}", url);
        let client = BaseNodeClient::connect(url.to_string()).await?;
        Ok(Self { client })
    }

    pub async fn get_version(&mut self) -> Result<u64, anyhow::Error> {
        let res = self
            .client
            .get_version(tonic::Request::new(Empty {}))
            .await?;
        // dbg!(res);
        Ok(0)
    }

    pub async fn get_block_template(&mut self) -> Result<NewBlockTemplateResponse, anyhow::Error> {
        let res = self
            .client
            .get_new_block_template(tonic::Request::new({
                NewBlockTemplateRequest {
                    max_weight: 0,
                    algo: Some(PowAlgo {
                        pow_algo: PowAlgos::Sha3x.into(),
                    }),
                }
            }))
            .await?;
        Ok(res.into_inner())
    }

    pub async fn get_new_block(
        &mut self,
        template: NewBlockTemplate,
    ) -> Result<GetNewBlockResult, anyhow::Error> {
        let res = self
            .client
            .get_new_block(tonic::Request::new(template))
            .await?;
        Ok(res.into_inner())
    }

    pub async fn submit_block(&mut self, block: Block) -> Result<(), anyhow::Error> {
        // dbg!(&block);
        let res = self.client.submit_block(tonic::Request::new(block)).await?;
        println!("Block submitted: {:?}", res);
        Ok(())
    }
}
