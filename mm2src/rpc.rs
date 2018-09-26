/******************************************************************************
 * Copyright © 2014-2018 The SuperNET Developers.                             *
 *                                                                            *
 * See the AUTHORS, DEVELOPER-AGREEMENT and LICENSE files at                  *
 * the top-level directory of this distribution for the individual copyright  *
 * holder information and the developer policies on copyright and licensing.  *
 *                                                                            *
 * Unless otherwise agreed in a custom licensing agreement, no part of the    *
 * SuperNET software, including this file may be copied, modified, propagated *
 * or distributed except according to the terms contained in the LICENSE file *
 *                                                                            *
 * Removal or modification of this copyright notice is prohibited.            *
 *                                                                            *
 ******************************************************************************/
//
//  rpc.rs
//
//  Copyright © 2014-2018 SuperNET. All rights reserved.
//
use futures::{self, Future};
use futures_cpupool::{CpuPool};
use helpers::{lp, MmArc, free_c_ptr, CORE};
use gstuff::{self};
use hyper::{self, Response, Request, Body, Method};
use hyper::server::conn::Http;
use hyper::rt::{Stream};
use hyper::service::Service;
use hyper::header::{HeaderValue, CONTENT_TYPE};
use lp_native_dex::MM_CTX_MAP;
use serde_json::{self as json, Value as Json};
use std::ffi::{CStr, CString};
use std::net::{SocketAddr};
use std::ptr::null_mut;
use std::os::raw::{c_char, c_void};
use std::str::from_utf8;
use std::thread;
use super::CJSON;
use tokio_core::net::TcpListener;

lazy_static! {
    /// Shared HTTP server.
    pub static ref HTTP: Http = Http::new();
    /// Shared CPU pool to run intensive/sleeping requests on separate thread
    pub static ref CPUPOOL: CpuPool = CpuPool::new(8);
}

const STATS_VALID_METHODS : &[&str; 14] = &[
    "psock", "ticker", "balances", "getprice", "notify", "getpeers", "orderbook",
    "statsdisp", "fundvalue", "help", "getcoins", "pricearray", "balance", "tradesarray"
];

fn lp_valid_remote_method(method: &str) -> bool {
    STATS_VALID_METHODS.iter().position(|&s| s == method).is_some()
}

macro_rules! unwrap_or_err_response {
    ($e:expr, $($args:tt)*) => {
        match $e {
            Ok(ok) => ok,
            Err(_e) => {
                return Ok(err_response($($args)*))
            }
        }
    }
}

macro_rules! unwrap_or_err_msg {
    ($e:expr, $($args:tt)*) => {
        match $e {
            Ok(ok) => ok,
            Err(_e) => {
                return Ok(err_to_json_string($($args)*))
            }
        }
    }
}

#[derive(Serialize)]
struct ErrResponse {
    error: String,
}

struct RpcService {
    /// The MmCtx id
    mm_ctx_id: u32,
    /// The socket of the original request is coming from.
    remote_sock: SocketAddr,
}

fn rpc_process_json(ctx: MmArc, remote_sock: SocketAddr, json: Json)
                        -> Result<String, String> {
    let body_json = unwrap_or_err_msg!(CJSON::from_str(&json.to_string()),
                                        "Couldn't parse request body as json");
    if !remote_sock.ip().is_loopback() && !lp_valid_remote_method(json["method"].as_str().unwrap()) {
        return Ok(err_to_json_string("Selected method can be called from localhost only!"));
    }

    if !json["queueid"].is_null() {
        if json["queueid"].is_u64() {
            if unsafe { lp::IPC_ENDPOINT == -1 } {
                return Ok(err_to_json_string("Can't queue the command when ws endpoint is disabled!"));
            } else if !remote_sock.ip().is_loopback() {
                return Ok(err_to_json_string("Can queue the command from localhost only!"));
            } else {
                let json_str = json.to_string();
                let c_json_ptr = unwrap_or_err_msg!(CString::new(json_str), "Error occurred").into_raw();
                unsafe {
                    lp::LP_queuecommand(null_mut(),
                                        c_json_ptr,
                                        lp::IPC_ENDPOINT,
                                        1,
                                        json["queueid"].as_u64().unwrap() as u32
                    );
                    CString::from_raw(c_json_ptr);
                }
                return Ok(r#"{"result":"success","status":"queued"}"#.to_string());
            }
        } else {
            return Ok(err_to_json_string("queueid must be unsigned integer!"));
        }
    }

    let my_ip_ptr = unwrap_or_err_msg!(CString::new(format!("{}", ctx.get_socket().ip())),
                                        "Error occurred");
    let remote_ip_ptr = unwrap_or_err_msg!(CString::new(format!("{}", remote_sock.ip())),
                                        "Error occurred");
    let stats_result = unsafe {
        lp::stats_JSON(
            ctx.btc_ctx() as *mut c_void,
            0,
            my_ip_ptr.as_ptr() as *mut c_char,
            -1,
            body_json.0,
            remote_ip_ptr.as_ptr() as *mut c_char,
            ctx.get_socket().port()
        )
    };

    if !stats_result.is_null() {
        let res_str = unsafe {
            unwrap_or_err_msg!(CStr::from_ptr(stats_result).to_str(),
            "Request execution result is empty")
        };
        let res_str = String::from (res_str);
        free_c_ptr(stats_result as *mut c_void);
        Ok(res_str)
    } else {
        Ok(err_to_json_string("Request execution result is empty"))
    }
}

fn rpc_response<T>(status: u16, body: T) -> Response<Body>
    where Body: From<T> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(Body::from(body))
        .unwrap()
}

fn err_to_json_string(err: &str) -> String {
    let err = ErrResponse {
        error: err.to_owned(),
    };
    json::to_string(&err).unwrap()
}

fn err_response(status: u16, msg: &str) -> Response<Body> {
    rpc_response(status, err_to_json_string(msg))
}

impl Service for RpcService {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = hyper::Error;
    type Future = Box<Future<Error=hyper::Error, Item=Response<Body>> + Send>;

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        if request.method() != Method::POST {
            return Box::new(
                futures::future::ok(err_response(400, "Only POST requests are supported!"))
            );
        }
        let body_f = request.into_body().concat2();

        let ctx = unwrap!(MM_CTX_MAP.read().unwrap().get(&self.mm_ctx_id)).clone();;
        let remote_sock = self.remote_sock.clone();
        Box::new(body_f.then(move |body| -> Result<Response<Body>, hyper::Error> {
            let body_vec = unwrap_or_err_response!(
                body,
                400,
                "Could not read request body"
            ).to_vec();

            let body_str = unwrap_or_err_response!(
                from_utf8(&body_vec),
                400,
                "Non-utf8 character in request body?"
            );

            let json : Json = unwrap_or_err_response!(
                json::from_str(body_str),
                400,
                "Could not parse request body as JSON"
            );

            if !json["method"].is_string() { return Ok(err_response(400, "Method is not set!")); }

            match json["method"].as_str() {
                Some("version") => {
                    let process = unwrap_or_err_response!(
                        rpc_process_json(ctx, remote_sock, json),
                        500,
                        "Error occurred"
                    );
                    Ok(rpc_response(200, process))
                },
                _ => {
                    let cpu_pool_fut = CPUPOOL.spawn_fn(move ||
                        rpc_process_json(ctx, remote_sock, json)
                    );
                    Ok(rpc_response(200, Body::wrap_stream(cpu_pool_fut.into_stream())))
                }
            }
        }))
    }
}

#[no_mangle]
pub extern "C" fn spawn_rpc_thread(mm_ctx_id: u32) {
    unwrap!(
        thread::Builder::new().name("mm_rpc".into()).spawn(move || {
            let ctx = unwrap!(MM_CTX_MAP.read().unwrap().get(&mm_ctx_id)).clone();
            let my_socket = ctx.get_socket().clone();

            let listener = unwrap!(
                TcpListener::bind2(ctx.get_socket().into()),
                "Could not bind socket for RPC server!"
            );

            let server = listener
                .incoming()
                .for_each(move |(socket, _my_sock)| {
                    let remote_sock = socket.peer_addr().unwrap();
                    CORE.spawn(move |_|
                        HTTP.serve_connection(
                            socket,
                            RpcService {
                                mm_ctx_id,
                                remote_sock
                            },
                        ).map(|_| ())
                            .map_err(|_| ())
                    );
                    Ok(())
                }).map_err(|e| panic!("accept error: {}", e));

            CORE.spawn(move |_| {
                println!(">>>>>>>>>> DEX stats {}:{} DEX stats API enabled at unixtime.{} <<<<<<<<<",
                         my_socket.ip(),
                         my_socket.port(),
                         gstuff::now_float() as u64
                );
                server
            });
        }),
        "Could not spawn RPC thread!"
    );
}
