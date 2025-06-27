use tonic::transport::{Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use futures::StreamExt;
use tokio::sync::mpsc;
use std::time::Duration;
use tokio::time::sleep;
use std::fs::File;
use std::io::BufReader;
use rustls_pemfile::{certs, pkcs8_private_keys};
use rustls::{Certificate, PrivateKey, ServerConfig};
use std::env;

// 导入生成的gRPC代码
use server_agent::server_agent_service_server::{ServerAgentService, ServerAgentServiceServer};
use server_agent::{ConnectRequest, CommandRequest, StatusReport, StatusResponse, CommandReply, Empty};

pub mod server_agent {
    tonic::include_proto!("server_agent");
}

// 客户端状态
#[derive(Debug, Clone)]
struct ClientState {
    client_id: String,
    hostname: String,
    os: String,
    version: String,
    last_status: Option<StatusReport>,
    command_sender: Option<mpsc::Sender<CommandRequest>>,
}

use std::collections::HashMap as SyncHashMap;
use tokio::sync::RwLock;

// 命令结果存储
#[derive(Debug, Clone, Default)]
struct CommandResultStore {
    results: SyncHashMap<String, CommandReply>,
}

impl CommandResultStore {
    fn new() -> Self {
        Self::default()
    }

    fn store_result(&mut self, command_id: String, result: CommandReply) {
        self.results.insert(command_id, result);
    }

    fn get_result(&self, command_id: &str) -> Option<&CommandReply> {
        self.results.get(command_id)
    }
}

// 服务端状态
struct ServerState {
    clients: Mutex<HashMap<String, ClientState>>,
    token_whitelist: Vec<String>,
    command_results: CommandResultStore,
}

impl ServerState {
    fn new(token_whitelist: Vec<String>) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            token_whitelist,
            command_results: CommandResultStore::new(),
        }
    }
    
    // 验证token是否有效
    fn is_token_valid(&self, token: &str) -> bool {
        self.token_whitelist.contains(&token.to_string())
    }
    
    // 添加或更新客户端状态
    fn update_client(&self, client_state: ClientState) {
        let mut clients = self.clients.lock().unwrap();
        clients.insert(client_state.client_id.clone(), client_state);
    }
    
    // 移除客户端
    fn remove_client(&self, client_id: &str) {
        let mut clients = self.clients.lock().unwrap();
        clients.remove(client_id);
    }
    
    // 获取所有客户端状态
    fn get_all_clients(&self) -> Vec<ClientState> {
        let clients = self.clients.lock().unwrap();
        clients.values().cloned().collect()
    }
    
    // 获取特定客户端
    fn get_client(&self, client_id: &str) -> Option<ClientState> {
        let clients = self.clients.lock().unwrap();
        clients.get(client_id).cloned()
    }
    
    // 存储命令结果
    fn store_command_result(&mut self, command_id: String, result: CommandReply) {
        self.command_results.store_result(command_id, result);
    }
}

#[tonic::async_trait]
impl ServerAgentService for Arc<ServerState> {
    // 客户端连接流 - 双向流
    type ConnectStream = mpsc::Receiver<CommandRequest>;
    
    async fn connect(
        &self,
        request: Request<ConnectRequest>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let connect_request = request.into_inner();
        log::info!("Connecting client: {:?}", connect_request);
        
        // 验证token
        if !self.is_token_valid(&connect_request.token) {
            log::error!("Invalid token from client: {}", connect_request.client_id);
            return Err(Status::unauthenticated("Invalid token"));
        }
        
        // 创建命令发送通道
        let (command_sender, command_receiver) = mpsc::channel(100);
        
        // 存储客户端状态
        let client_state = ClientState {
            client_id: connect_request.client_id.clone(),
            hostname: connect_request.hostname.clone(),
            os: connect_request.os.clone(),
            version: connect_request.version.clone(),
            last_status: None,
            command_sender: Some(command_sender.clone()),
        };
        self.update_client(client_state);
        
        log::info!("Client connected successfully: {}", connect_request.client_id);
        
        // 创建响应流
        let response = Ok(Response::new(command_receiver));

        // 当连接关闭时清理客户端状态
        tokio::spawn({ 
            let server_state = self.clone();
            let client_id = connect_request.client_id.clone();
            async move {
                // 等待流结束
                tokio::time::sleep(Duration::from_secs(1)).await;
                server_state.remove_client(&client_id);
                log::info!("Client disconnected: {}", client_id);
            }
        });

        response
    }
    
    // 处理客户端状态报告
    async fn report_status(
        &self,
        request: Request<StatusReport>,
    ) -> Result<Response<StatusResponse>, Status> {
        let status_report = request.into_inner();
        log::debug!("Received status report from {}: {:?}", status_report.client_id, status_report);
        
        // 更新客户端状态
        if let Some(mut client) = self.get_client(&status_report.client_id) {
            client.last_status = Some(status_report.clone());
            self.update_client(client);
        }
        
        Ok(Response::new(StatusResponse {
            success: true,
            message: "Status received".to_string(),
        }))
    }
    
    // 处理命令执行响应
    async fn command_response(
        &self,
        request: Request<CommandReply>,
    ) -> Result<Response<Empty>, Status> {
        let command_reply = request.into_inner();
        log::info!("Received command response for {}: exit_code={}", command_reply.command_id, command_reply.exit_code);
        
        // 存储命令结果
        self.store_command_result(command_reply.command_id.clone(), command_reply.clone());
        
        // 打印命令执行结果
        println!();
        println!("命令执行结果 (ID: {})", command_reply.command_id);
        println!("退出码: {}", command_reply.exit_code);
        if !command_reply.stdout.is_empty() {
            println!("标准输出:");
            println!("{}", command_reply.stdout);
        }
        if !command_reply.stderr.is_empty() {
            println!("错误输出:");
            println!("{}", command_reply.stderr);
        }
        println!("----------------------------------------");
        print!("$ ");
        
        Ok(Response::new(Empty {}))
    }
    
    // 处理Ping请求用于延迟测量
    async fn ping(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Empty>, Status> {
        Ok(Response::new(Empty {}))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    log::info!("Starting ServerAgent server...");
    
    // 从环境变量加载允许的token列表
    dotenv::dotenv().ok();
    let token_whitelist = std::env::var("TOKEN_WHITELIST")
        .unwrap_or_else(|_| "valid_token_123".to_string())
        .split(',')
        .map(|s| s.to_string())
        .collect();
    
    // 创建服务状态
    let server_state = Arc::new(ServerState::new(token_whitelist));
    
    // 配置服务器地址
    let addr = "0.0.0.0:50051".parse()?;
    log::info!("Server listening on {}", addr);
    
    // 创建gRPC服务
    let server = ServerAgentServiceServer::new(server_state.clone());
    
    // 在后台启动服务器
    tokio::spawn(async move {
        // 加载TLS证书和私钥
        let cert_file = File::open("server.pem").expect("Failed to open certificate file");
        let key_file = File::open("server-key.pem").expect("Failed to open private key file");

        let cert_chain = certs(&mut BufReader::new(cert_file))
            .expect("Failed to read certificate chain")
            .into_iter()
            .map(|cert| rustls::Certificate(cert))
            .collect();

        let mut keys = pkcs8_private_keys(&mut BufReader::new(key_file))
            .expect("Failed to read private key")
            .into_iter()
            .map(|key| rustls::PrivateKey(key))
            .collect::<Vec<_>>();

        if keys.is_empty() {
            panic!("No private keys found in key file");
        }

        let tls_config = ServerTlsConfig::builder()
            .identity(rustls::ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(cert_chain, keys.remove(0))
                .expect("Failed to create server TLS configuration"))
            .into();

        Server::builder()
            .tls_config(tls_config)
            .expect("Failed to configure TLS")
            .add_service(server)
            .serve(addr)
            .await.expect("Server failed to start");
    });
    
    // 启动交互式CLI
    start_interactive_cli(server_state).await;
    
    Ok(())
}

// 交互式CLI
async fn start_interactive_cli(server_state: Arc<ServerState>) {
    println!();
    println!("ServerAgent 控制台");
    println!("-----------------");
    println!("可用命令:");
    println!("  list    - 列出所有连接的客户端");
    println!("  select <client_id> - 选择客户端");
    println!("  exit    - 退出服务器");
    println!();
    
    let mut input = String::new();
    let mut selected_client = None;
    
    loop {
        // 显示提示符
        if let Some(client_id) = &selected_client {
            print!("[{}] $ ", client_id);
        } else {
            print!("$ ");
        }
        std::io::stdout().flush().unwrap();
        
        // 读取输入
        input.clear();
        std::io::stdin().read_line(&mut input).unwrap();
        let input = input.trim();
        
        // 解析命令
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        
        match parts[0] {
            "list" => {
                list_clients(&server_state);
            }
            "select" => {
                if parts.len() < 2 {
                    println!("请指定客户端ID");
                    continue;
                }
                selected_client = select_client(&server_state, parts[1]);
            }
            "exit" => {
                println!("正在关闭服务器...");
                break;
            }
            _ => {
                if let Some(client_id) = &selected_client {
                    send_command(&server_state, client_id, input).await;
                } else {
                    println!("未知命令。请先使用 'select <client_id>' 选择客户端或 'list' 查看客户端列表");
                }
            }
        }
    }
}

// 列出所有客户端
fn list_clients(server_state: &Arc<ServerState>) {
    let clients = server_state.get_all_clients();
    
    if clients.is_empty() {
        println!("没有连接的客户端");
        return;
    }
    
    println!("连接的客户端 ({}):", clients.len());
    println!("{:<30} {:<15} {:<10} {:<10}", "客户端ID", "主机名", "操作系统", "版本");
    println!("{:-<30} {:-<15} {:-<10} {:-<10}", "", "", "", "");
    
    for client in clients {
        println!("{:<30} {:<15} {:<10} {:<10}", 
                 client.client_id, client.hostname, client.os, client.version);
    }
}

// 选择客户端
fn select_client(server_state: &Arc<ServerState>, client_id: &str) -> Option<String> {
    let clients = server_state.get_all_clients();
    
    if let Some(client) = clients.iter().find(|c| c.client_id == client_id) {
        println!("已选择客户端: {} ({})", client.client_id, client.hostname);
        Some(client.client_id.clone())
    } else {
        println!("未找到客户端: {}", client_id);
        None
    }
}

// 发送命令
async fn send_command(server_state: &Arc<ServerState>, client_id: &str, command: &str) {
    let client = server_state.get_client(client_id);
    if let Some(client) = client {
        if let Some(sender) = &client.command_sender {
            let command_id = format!("cmd-{}", chrono::Utc::now().timestamp_millis());
            
            let cmd_request = CommandRequest {
                command_id: command_id.clone(),
                command: command.to_string(),
                interactive: false,
            };
            
            match sender.send(cmd_request).await {
                Ok(_) => println!("命令已发送 (ID: {})", command_id),
                Err(e) => {
                    println!("发送命令失败: {:?}", e);
                    // 移除无响应的客户端
                    server_state.remove_client(client_id);
                }
            }
        } else {
            println!("无法发送命令: 客户端命令通道不存在");
            server_state.remove_client(client_id);
        }
    } else {
        println!("客户端已断开连接");
    }
}