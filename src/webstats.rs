//! 只读本机统计页：同源静态资源和 API，任务控制与评分仍通过用户专用命名管道。
use crate::ipc::{Envelope, Request};
use anyhow::{Context, Result};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::JoinHandle,
    time::Duration,
};
use tiny_http::{Header, Method, Response, StatusCode};

/// HTTP 监听只绑定回环地址；随机路径仅由当前管理器 UI/用户管道取得。
pub struct WebStats {
    pub url: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl WebStats {
    /// 启动轻量只读服务，不读取任务日志；快照通过管理泵获得一致状态。
    pub fn start(commands: Sender<Envelope>) -> Result<Self> {
        let server = tiny_http::Server::http(("127.0.0.1", 0))
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let host = server
            .server_addr()
            .to_ip()
            .context("HTTP loopback address missing")?
            .to_string();
        let prefix = format!("/{}/", uuid::Uuid::new_v4().simple());
        let url = format!("http://{host}{prefix}");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(request)) => respond(request, &host, &prefix, &commands),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            url,
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for WebStats {
    /// 管理页结束时停止监听；正在等快照的请求最多等待两秒。
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 校验请求的访问边界；页面没有写入接口，也不响应跨站读取。
fn permitted(request: &tiny_http::Request, host: &str, prefix: &str) -> bool {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str())
    };
    request.method() == &Method::Get
        && header("Host") == Some(host)
        && header("Origin").is_none_or(|origin| origin == format!("http://{host}"))
        && request.url().starts_with(prefix)
}

/// 只返回内嵌资源或审计指标，禁止路径穿越、文件执行及任意文件读取。
fn respond(request: tiny_http::Request, host: &str, prefix: &str, commands: &Sender<Envelope>) {
    if !permitted(&request, host, prefix) {
        send(
            request,
            403,
            "text/plain; charset=utf-8",
            b"Forbidden".to_vec(),
        );
        return;
    }
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or("")
        .strip_prefix(prefix)
        .unwrap_or("");
    let (status, kind, body) = match path {
        "" | "index.html" => (
            200,
            "text/html; charset=utf-8",
            include_bytes!("../assets/statistics.html").to_vec(),
        ),
        "statistics.css" => (
            200,
            "text/css; charset=utf-8",
            include_bytes!("../assets/statistics.css").to_vec(),
        ),
        "statistics.js" => (
            200,
            "text/javascript; charset=utf-8",
            include_bytes!("../assets/statistics.js").to_vec(),
        ),
        "icon.png" => (
            200,
            "image/png",
            include_bytes!("../assets/window.png").to_vec(),
        ),
        "api/statistics" => {
            let (reply, receiver) = mpsc::sync_channel(1);
            let response = commands
                .send(Envelope {
                    request: Request::StatisticsData,
                    reply,
                })
                .ok()
                .and_then(|_| receiver.recv_timeout(Duration::from_secs(2)).ok())
                .unwrap_or_else(|| json!({"type":"error","error_code":"manager_unavailable"}));
            let status = if response["type"] == "error" {
                503
            } else {
                200
            };
            (
                status,
                "application/json; charset=utf-8",
                serde_json::to_vec(&response).expect("statistics serialization"),
            )
        }
        _ => (404, "text/plain; charset=utf-8", b"Not found".to_vec()),
    };
    send(request, status, kind, body);
}

/// 安全头与资源同源策略统一设置；浏览器断开只影响自身，不中断管理器。
fn send(request: tiny_http::Request, status: u16, content_type: &str, body: Vec<u8>) {
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    for (key, value) in [
        ("Content-Type", content_type),
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
        ("Cross-Origin-Resource-Policy", "same-origin"),
        (
            "Content-Security-Policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
        ),
    ] {
        response.add_header(Header::from_bytes(key, value).expect("static HTTP header"));
    }
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    /// 用真实 HTTP 请求验证同源访问和只读边界，不需要浏览器或用户桌面。
    #[test]
    fn local_http_is_read_only_and_protected() {
        let (commands, requests) = mpsc::channel::<Envelope>();
        let server = WebStats::start(commands).unwrap();
        let host_path = server.url.strip_prefix("http://").unwrap();
        let (host, path) = host_path.split_once('/').unwrap();
        let worker = std::thread::spawn(move || {
            let envelope = requests.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(envelope.request, Request::StatisticsData));
            envelope
                .reply
                .send(json!({"schema_version":1,"sessions":[]}))
                .unwrap();
        });
        let query = |method: &str, path: &str, request_host: &str, origin: &str| {
            let mut stream = std::net::TcpStream::connect(host).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            write!(stream,"{method} {path} HTTP/1.1\r\nHost: {request_host}\r\nConnection: close\r\n{origin}\r\n").unwrap();
            let mut data = String::new();
            stream.read_to_string(&mut data).unwrap();
            data
        };
        assert!(query("GET", "/api/statistics", host, "").starts_with("HTTP/1.1 403"));
        assert!(
            query("POST", &format!("/{path}api/statistics"), host, "").starts_with("HTTP/1.1 403")
        );
        assert!(
            query("GET", &format!("/{path}api/statistics"), "outside.test", "")
                .starts_with("HTTP/1.1 403")
        );
        assert!(
            query(
                "GET",
                &format!("/{path}api/statistics"),
                host,
                "Origin: https://outside.test\r\n"
            )
            .starts_with("HTTP/1.1 403")
        );
        let data = query("GET", &format!("/{path}api/statistics"), host, "");
        assert!(data.starts_with("HTTP/1.1 200"));
        assert!(data.contains("\"sessions\":[]"));
        let page = query("GET", &format!("/{path}"), host, "");
        assert!(page.starts_with("HTTP/1.1 200"));
        assert!(
            page.to_ascii_lowercase()
                .contains("content-security-policy")
        );
        worker.join().unwrap();
    }
}
