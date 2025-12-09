mod config;
mod crypto;
mod network;
mod blockchain;

use clap::{Parser, Subcommand};
use crate::config::load;

#[derive(Parser)]
#[command(name = "MBPChannel")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动 Operator 节点
    Operator,
    /// 启动 User 节点
    User { 
        /// 用户名 (必须在 config.toml 中存在)
        name: String,
        
        /// [新增] 初始锁币金额 (单位: wei)，可选。如果不填默认 10 wei
        /// 用法: cargo run -- user alice --amount 1000
        #[arg(short, long)] 
        amount: Option<u128> 
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = load()?;

    match cli.command {
        Commands::Operator => {
            if let Err(e) = network::operator::run(config).await {
                eprintln!("❌ Operator 发生严重错误: {}", e);
            }
        }
        // [修改] 这里解构出新增的 amount 参数
        Commands::User { name, amount } => {
            if config.get_user_port(&name).is_none() {
                eprintln!("❌ 错误: 在 config.toml 中找不到用户 '{}'", name);
                return Ok(());
            }
            
            // [修改] 将 amount (Option<u128>) 传递给 user::run
            // 如果命令行没输 --amount，这里就是 None，user.rs 会自动处理为默认值
            if let Err(e) = network::user::run(config, name.clone(), amount).await {
                eprintln!("❌ User '{}' 发生错误: {}", name, e);
            }
        }
    }
    Ok(())
}