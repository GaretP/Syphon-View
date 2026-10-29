//! Used by tests/vpn_e2e.sh: the app's helper without its window. Prints "<port> <key>" and runs until killed.
#![allow(dead_code)]
#[path = "../src/discover.rs"]
mod discover;
#[path = "../src/proxy.rs"]
mod proxy;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/vpn.rs"]
mod vpn;
#[path = "../src/wake.rs"]
mod wake;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data = std::path::PathBuf::from(std::env::args().nth(1).expect("data folder"));
    let st = proxy::AppState::new(data, None, std::sync::Arc::new(|_| {}));
    let port = proxy::serve(st.clone()).await?;
    println!("{port} {}", st.key);
    std::future::pending::<()>().await;
    Ok(())
}
