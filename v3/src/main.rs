mod domain;
use serde_json::Value;
use tezbar_extension_sdk::{BoxFuture, CoreResult, Extension, ExtensionContext};
struct Calendar;
impl Extension for Calendar {
    fn id(&self) -> &'static str {
        "com.tezbar.native.google-calendar"
    }
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    fn commands(&self) -> &'static [&'static str] {
        domain::commands()
    }
    fn initialize<'a>(&'a self, ctx: &'a ExtensionContext) -> BoxFuture<'a, CoreResult> {
        Box::pin(domain::initialize(ctx))
    }
    fn handle<'a>(
        &'a self,
        ctx: &'a ExtensionContext,
        command: &'a str,
        operation: &'a str,
        payload: &'a Value,
        state: &'a mut Value,
    ) -> BoxFuture<'a, CoreResult> {
        Box::pin(domain::handle(ctx, command, operation, payload, state))
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    if std::env::args().any(|arg| arg == "--build-info") {
        println!(
            "{}",
            serde_json::json!({"fixtureNetwork":cfg!(feature = "fixture-network"), "targetOs":std::env::consts::OS, "targetArch":std::env::consts::ARCH})
        );
        return;
    }
    if tezbar_extension_sdk::serve(Calendar).await.is_err() {
        eprintln!("Google Calendar protocol failed");
        std::process::exit(1);
    }
}
