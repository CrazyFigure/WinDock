//! 用系统 WinHTTP 跟随代理与 HTTPS 重定向，保留原生程序的依赖规模。
//! 下载支持流式回调（64 KB 有限缓冲），安装包不整体读入内存。

use std::{ffi::c_void, ptr};

use windows::core::{w, Error, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

use crate::win32::wide;

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void) -> windows::core::Result<Self> {
        if raw.is_null() {
            return Err(Error::from_win32());
        }
        Ok(Self(raw))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// 持有一次请求的完整 WinHTTP 句柄链与响应元信息。
struct Response {
    // Rust 按字段声明顺序析构：必须先关闭子请求句柄，再依次关闭连接与会话句柄，
    // 否则关闭父句柄会取消未读完的请求并导致 WinHttpReadData 报 0x80072EF1。
    request: Handle,
    _connection: Handle,
    _session: Handle,
    status: u32,
    content_length: Option<u64>,
}

/// 建立连接并发送 GET，返回完整句柄链与响应状态。
fn open_get(agent: &str, host: &str, path: &str, accept: PCWSTR) -> Result<Response, String> {
    let host = wide(host);
    let path = wide(path);
    let agent = wide(agent);
    unsafe {
        let session = Handle::new(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))
        .map_err(|err| err.to_string())?;
        WinHttpSetTimeouts(session.0, 5_000, 8_000, 12_000, 20_000).map_err(|err| err.to_string())?;
        let connection = Handle::new(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), 443, 0)).map_err(|err| err.to_string())?;
        let types = [accept, PCWSTR::null()];
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            types.as_ptr(),
            WINHTTP_FLAG_SECURE,
        ))
        .map_err(|err| err.to_string())?;
        let redirect = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP.to_ne_bytes();
        WinHttpSetOption(Some(request.0), WINHTTP_OPTION_REDIRECT_POLICY, Some(&redirect)).map_err(|err| err.to_string())?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|err| err.to_string())?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).map_err(|err| err.to_string())?;
        let mut status = 0u32;
        let mut status_bytes = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(ptr::from_mut(&mut status).cast()),
            &mut status_bytes,
            ptr::null_mut(),
        )
        .map_err(|err| err.to_string())?;
        let mut len = 0u32;
        let mut size = size_of::<u32>() as u32;
        let has_len = WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(ptr::from_mut(&mut len).cast()),
            &mut size,
            ptr::null_mut(),
        )
        .is_ok();
        Ok(Response {
            request,
            _connection: connection,
            _session: session,
            status,
            content_length: has_len.then_some(len as u64),
        })
    }
}

fn read_body(resp: &Response, limit: u64, mut on_chunk: impl FnMut(&[u8], u64, Option<u64>) -> Result<(), String>) -> Result<(), String> {
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let mut read = 0u32;
        unsafe {
            WinHttpReadData(resp.request.0, buffer.as_mut_ptr().cast(), buffer.len() as u32, &mut read)
                .map_err(|err| err.to_string())?;
        }
        if read == 0 {
            return Ok(());
        }
        total += read as u64;
        if total > limit {
            return Err("发布服务器返回的数据超过大小限制".into());
        }
        on_chunk(&buffer[..read as usize], total, resp.content_length)?;
    }
}

/// 读取完整响应（用于小型 JSON / 签名文件）。
pub fn get(host: &str, path: &str, accept: PCWSTR, limit: usize) -> Result<Vec<u8>, String> {
    let resp = open_get("WinDockFinder", host, path, accept)?;
    if resp.status != 200 {
        return Err(format!("HTTP {}", resp.status));
    }
    let mut out = Vec::new();
    read_body(&resp, limit as u64, |chunk, _, _| {
        out.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(out)
}

/// 流式下载：每个 64 KB 块回调一次（累计字节、总长度），调用方边写盘边计算 SHA-256。
pub fn stream(host: &str, path: &str, limit: u64, on_chunk: impl FnMut(&[u8], u64, Option<u64>) -> Result<(), String>) -> Result<(), String> {
    let resp = open_get("WinDockFinder", host, path, w!("application/octet-stream"))?;
    if resp.status != 200 {
        return Err(format!("HTTP {}", resp.status));
    }
    read_body(&resp, limit, on_chunk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn response_fields_drop_child_before_parents() {
        // 验证 Response 结构体保持 request -> _connection -> _session 的析构顺序，
        // 避免 Drop 时先关闭父句柄导致 WinHTTP 取消子请求。
        struct Probe<'a> {
            name: &'static str,
            order: &'a RefCell<Vec<&'static str>>,
        }
        impl Drop for Probe<'_> {
            fn drop(&mut self) {
                self.order.borrow_mut().push(self.name);
            }
        }
        struct OrderCheck<'a> {
            _request: Probe<'a>,
            _connection: Probe<'a>,
            _session: Probe<'a>,
        }
        let log = RefCell::new(Vec::new());
        {
            let _check = OrderCheck {
                _request: Probe { name: "request", order: &log },
                _connection: Probe { name: "connection", order: &log },
                _session: Probe { name: "session", order: &log },
            };
        }
        assert_eq!(*log.borrow(), vec!["request", "connection", "session"]);
        assert!(std::mem::offset_of!(Response, request) < std::mem::offset_of!(Response, _connection));
        assert!(std::mem::offset_of!(Response, _connection) < std::mem::offset_of!(Response, _session));
    }
}
