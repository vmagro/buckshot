use service::HelloReply;
use service::HelloRequest;
use service::greeter_server::Greeter;

struct GreeterImpl;

#[tonic::async_trait]
impl Greeter for GreeterImpl {
    async fn hello(
        &self,
        request: tonic::Request<HelloRequest>,
    ) -> Result<tonic::Response<HelloReply>, tonic::Status> {
        Ok(tonic::Response::new(HelloReply {
            greeting: format!("hello, {}!", request.into_inner().name),
        }))
    }
}

#[test]
fn service_trait_is_implemented() {
    fn assert_greeter<T: Greeter>() {}
    assert_greeter::<GreeterImpl>();
}
