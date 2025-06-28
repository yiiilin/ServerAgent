use clap::{arg, command, Parser};
use futures::StreamExt;
use network_interface::NetworkInterfaceConfig;
use std::fs;
use std::time::{Duration, Instant};
use sysinfo::{CpuExt, DiskExt, NetworkExt, RefreshKind, SystemExt};
use tokio::time::sleep;
use tonic::transport::{Certificate, Channel, Uri};
use tonic::Request;

// 导入生成的gRPC代码
use server_agent::server_agent_service_client::ServerAgentServiceClient;
use server_agent::{ConnectRequest, Empty, StatusReport};

pub mod server_agent {
    tonic::include_proto!("server_agent");
}

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Server Agent",
    long_about = "A terminal proxy based on grpc, which uses grpc to communicate with the outside world and implement terminal command execution, file transfer and other functions"
)]
#[derive(Debug)]
struct Arguments {
    #[arg(short, long)]
    certpath: String,
    #[arg(short, long)]
    token: String,
    #[arg(short, long)]
    addr: String,
    #[arg(short, long, default_value = "60")]
    status_interval: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = Arguments::parse();
    log::info!("Starting client with args: {:?}", args);

    // 加载证书
    let cert = fs::read_to_string(&args.certpath)?;
    let cert = Certificate::from_pem(cert);

    // 创建gRPC通道
    let channel = Channel::builder(args.addr.parse::<Uri>()?)
        .tls_config(tonic::transport::ClientTlsConfig::new().ca_certificate(cert))?
        .connect()
        .await?;

    // 创建gRPC客户端
    let mut client = ServerAgentServiceClient::new(channel);

    // 握手连接
    let connect_request = Request::new(ConnectRequest {
        token: args.token.clone(),
        client_id: get_client_id()?,
        hostname: get_hostname()?,
        os: get_os_info()?,
        version: env!("CARGO_PKG_VERSION").to_string(),
    });

    log::info!("Connecting to server...");
    let mut command_stream = client.handshake(connect_request).await?.into_inner();
    log::info!("Successfully connected to server");

    // 启动状态上报任务
    let mut client_clone: ServerAgentServiceClient<Channel> = client.clone();
    let status_interval = args.status_interval;
    tokio::spawn(async move {
        loop {
            if let Err(e) = report_status(&mut client_clone).await {
                log::error!("Failed to report status: {:?}", e);
            }
            sleep(Duration::from_secs(status_interval)).await;
        }
    });

    // 处理命令流
    while let Some(command) = command_stream.next().await {
        match command {
            Ok(cmd) => {
                log::info!("Received command: {:?}", cmd);
                let cli = client.clone();
                tokio::spawn(async move {
                    let cli = cli;
                    if let Err(e) = execute_command(cmd, cli).await {
                        log::error!("Failed to execute command: {:?}", e);
                    }
                });
            }
            Err(e) => {
                log::error!("Command stream error: {:?}", e);
                break;
            }
        }
    }

    Ok(())
}

// 获取客户端唯一ID
fn get_client_id() -> Result<String, Box<dyn std::error::Error>> {
    // 实际实现应生成或获取唯一客户端ID
    Ok(format!("client-{}", get_hostname()?))
}

// 获取主机名
fn get_hostname() -> Result<String, Box<dyn std::error::Error>> {
    Ok(sysinfo::System::new()
        .host_name()
        .unwrap_or(String::from("hostname not found")))
}

// 获取操作系统信息
fn get_os_info() -> Result<String, Box<dyn std::error::Error>> {
    Ok(format!("{}", std::env::consts::OS))
}

// 上报客户端状态
async fn report_status(
    client: &mut ServerAgentServiceClient<Channel>,
) -> Result<(), Box<dyn std::error::Error>> {
    // 收集系统信息
    let hardware = collect_hardware_info()?;
    let network = collect_network_info()?;

    // 测量网络延迟
    let start_time = Instant::now();
    let ping_request = Request::new(Empty {});
    client.ping(ping_request).await?;
    let latency = start_time.elapsed().as_secs_f64() * 1000.0; // 转换为毫秒

    let status_report = StatusReport {
        client_id: get_client_id()?,
        timestamp: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
        hardware: Some(hardware),
        network: Some(network),
        latency,
    };

    log::debug!("Reporting status: {:?}", status_report);
    let response = client.report_status(status_report).await?;
    log::info!("Status reported successfully: {:?}", response.into_inner());

    Ok(())
}

// 收集硬件信息
fn collect_hardware_info() -> Result<server_agent::HardwareInfo, Box<dyn std::error::Error>> {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();

    // CPU信息
    let cpu_model = sys.cpus()[0].brand().to_string();
    let cores = sys.cpus().len() as i32;
    let load_avg = sys.load_average();

    // 内存信息
    let total_memory = sys.total_memory();
    let used_memory = sys.used_memory();
    let free_memory = sys.free_memory();
    let memory_usage = (used_memory as f64 / total_memory as f64) * 100.0;

    // 磁盘信息
    let mut disk_total = 0;
    let mut disk_used = 0;
    let mut disk_free = 0;
    for disk in sys.disks() {
        disk_total += disk.total_space();
        disk_used += disk.total_space() - disk.available_space();
        disk_free += disk.available_space();
    }
    let disk_usage = if disk_total > 0 {
        (disk_used as f64 / disk_total as f64) * 100.0
    } else {
        0.0
    };

    Ok(server_agent::HardwareInfo {
        cpu: Some(server_agent::CpuInfo {
            model: cpu_model,
            cores,
            usage: sys.global_cpu_info().cpu_usage().into(),
            load_avg_1: load_avg.one,
            load_avg_5: load_avg.five,
            load_avg_15: load_avg.fifteen,
        }),
        memory: Some(server_agent::MemoryInfo {
            total: total_memory,
            used: used_memory,
            free: free_memory,
            usage: memory_usage,
        }),
        disk: Some(server_agent::DiskInfo {
            total: disk_total,
            used: disk_used,
            free: disk_free,
            usage: disk_usage,
        }),
    })
}

// 收集网络信息
fn collect_network_info() -> Result<server_agent::NetworkInfo, Box<dyn std::error::Error>> {
    // 获取IP地址
    let interfaces = network_interface::NetworkInterface::show()?;
    let ip_address = interfaces
        .iter()
        .find(|iface| iface.name != "lo" && !iface.addr.is_empty())
        .and_then(|iface| iface.addr.first())
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    // 获取网络流量信息
    let sys = sysinfo::System::new_with_specifics(RefreshKind::new().with_networks());
    let networks = sys.networks();

    let (mut tx_bytes, mut rx_bytes) = (0, 0);
    for (_, data) in networks {
        tx_bytes += data.transmitted();
        rx_bytes += data.received();
    }

    Ok(server_agent::NetworkInfo {
        ip_address,
        tx_bytes,
        rx_bytes,
        tx_speed: 0, // 简化实现，实际项目中应计算速度
        rx_speed: 0, // 简化实现，实际项目中应计算速度
    })
}

// 执行命令
async fn execute_command(
    cmd: server_agent::CommandRequest,
    mut client: ServerAgentServiceClient<Channel>,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Executing command: {}", cmd.command);

    // 执行命令
    let output = if cfg!(target_os = "windows") {
        tokio::process::Command::new("cmd")
            .arg("/c")
            .arg(&cmd.command)
            .output()
            .await?
    } else {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd.command)
            .output()
            .await?
    };

    // 处理命令输出
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    // 发送命令执行结果
    let reply = server_agent::CommandReply {
        command_id: cmd.command_id,
        exit_code: exit_code as i32,
        stdout,
        stderr,
        completed: true,
    };

    client.command_response(reply).await?;
    Ok(())
}
