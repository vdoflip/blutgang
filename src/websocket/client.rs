use crate::{
    balancer::{
        format::replace_block_tags,
        processing::{
            cache_querry,
            update_rpc_latency,
            CacheArgs,
        },
        selection::select::pick,
    },
    rpc::types::Rpc,
    websocket::{
        error::Error,
        types::{
            SubscriptionData,
            WsChannelErr,
            WsconnMessage,
            IncomingResponse,
        },
    },
};

use std::{
    sync::{
        Arc,
        RwLock,
    },
    time::Instant,
};

use futures_util::{
    SinkExt,
    StreamExt,
};
use serde_json::Value;
use simd_json::from_slice;
use tokio::sync::{
    broadcast,
    mpsc,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::Message,
};

#[cfg(not(feature = "xxhash"))]
use blake3::hash;

#[cfg(feature = "xxhash")]
use xxhash_rust::xxh3::xxh3_64;

pub async fn ws_conn_manager(
    rpc_list: Arc<RwLock<Vec<Rpc>>>,
    mut incoming_rx: mpsc::UnboundedReceiver<WsconnMessage>,
    broadcast_tx: broadcast::Sender<IncomingResponse>,
    ws_error_tx: mpsc::UnboundedSender<WsChannelErr>,
) {
    let mut ws_handles = create_ws_vec(&rpc_list, &broadcast_tx, &ws_error_tx).await;

    while let Some(message) = incoming_rx.recv().await {
        match message {
            WsconnMessage::Message(incoming) => {
                if let Some(rpc_position) = {
                    let mut rpc_list_guard = rpc_list.write().unwrap();
                    pick(&mut rpc_list_guard).1
                } {
                    if rpc_position >= ws_handles.len() {
                        println!("ws_conn_manager error: rpc_position out of bounds");
                        continue;
                    }

                    if let Some(ws) = &ws_handles[rpc_position] {
                        if ws.send(incoming).is_err() {
                            println!("ws_conn_manager error: failed to send message");
                        }
                    } else {
                        println!("No WS connection at index {}", rpc_position);
                    }
                } else {
                    println!("ws_conn_manager error: no rpc_position");
                }
            }
            WsconnMessage::Reconnect() => {
                ws_handles = create_ws_vec(&rpc_list, &broadcast_tx, &ws_error_tx).await;
            }
        }
    }
}

pub async fn create_ws_vec(
    rpc_list: &Arc<RwLock<Vec<Rpc>>>,
    broadcast_tx: &broadcast::Sender<IncomingResponse>,
    ws_error_tx: &mpsc::UnboundedSender<WsChannelErr>,
) -> Vec<Option<mpsc::UnboundedSender<Value>>> {
    let rpc_list_clone = rpc_list.read().unwrap().clone();
    let mut ws_handles = Vec::new();

    for (index, rpc) in rpc_list_clone.iter().enumerate() {
        let (ws_conn_incoming_tx, ws_conn_incoming_rx) = mpsc::unbounded_channel();
        ws_handles.push(Some(ws_conn_incoming_tx));
        ws_conn(
            rpc.clone(),
            rpc_list.clone(),
            ws_conn_incoming_rx,
            broadcast_tx.clone(),
            ws_error_tx.clone(),
            index,
        )
        .await;
    }

    ws_handles
}

pub async fn ws_conn(
    rpc: Rpc,
    rpc_list: Arc<RwLock<Vec<Rpc>>>,
    mut incoming_rx: mpsc::UnboundedReceiver<Value>,
    broadcast_tx: broadcast::Sender<IncomingResponse>,
    ws_error_tx: mpsc::UnboundedSender<WsChannelErr>,
    index: usize,
) {
    let url = reqwest::Url::parse(&rpc.ws_url.unwrap()).expect("Failed to parse URL");
    let (mut ws_stream, _) = connect_async(url).await.expect("Failed to connect to WS");

    tokio::spawn(async move {
        while let Some(incoming) = incoming_rx.recv().await {
            #[cfg(feature = "debug-verbose")]
            println!("ws_conn[{}], result: {:?}", index, incoming);

            let time = Instant::now();
            match ws_stream.send(Message::Text(incoming.to_string())).await {
                Ok(_) => {}
                Err(_) => {
                    let _ = ws_error_tx.send(WsChannelErr::Closed(index));
                    break;
                }
            }

            match ws_stream.next().await.unwrap() {
                Ok(message) => {
                    let time = time.elapsed();
                    let rax =
                        unsafe { simd_json::from_str(&mut message.into_text().unwrap()).unwrap() };

                    let incoming = IncomingResponse {
                        node_id: index,
                        content: rax,
                    };

                    let _ = broadcast_tx.send(incoming);
                    update_rpc_latency(&rpc_list, index, time);
                }
                Err(_) => {
                    let _ = ws_error_tx.send(WsChannelErr::Closed(index));
                    break;
                }
            }
        }
    });
}

pub async fn execute_ws_call(
    mut call: Value,
    user_id: u64,
    incoming_tx: &mpsc::UnboundedSender<WsconnMessage>,
    broadcast_rx: broadcast::Receiver<IncomingResponse>,
    sub_data: &Arc<SubscriptionData>,
    cache_args: &CacheArgs,
) -> Result<String, Error> {
    let id = call["id"].take();
    let tx_hash = {
        #[cfg(not(feature = "xxhash"))]
        {
            hash(call.to_string().as_bytes())
        }
        #[cfg(feature = "xxhash")]
        {
            xxh3_64(call.to_string().as_bytes())
        }
    };

    if let Ok(Some(mut rax)) = cache_args.cache.get(tx_hash.as_bytes()) {
        let mut cached: Value = from_slice(&mut rax).unwrap();
        cached["id"] = id;
        return Ok(cached.to_string());
    }

    // Remove and unsubscribe user is "eth_unsubscribe"
    if call["method"] == "eth_unsubscribe" {
        // subscription_id is ["params"][0]
        let subscription_id = call["params"][0].as_str();
        let subscription_id = match subscription_id {
            Some(subscription_id) => subscription_id,
            None => {
                return Ok(
                    // TODO: change id
                    "\"jsonrpc\":\"2.0\", \"id\":1, \"error\": \"Bad Subscription ID!\""
                        .to_string(),
                );
            }
        };

        sub_data.unsubscribe_user(user_id, subscription_id.to_string());
        // TODO: change id
        return Ok("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":true}".to_string());
    }

    let is_subscription = call["method"] == "eth_subscribe";
    if is_subscription {
        // Check if we're already subscribed to this
        // if so return the subscription id and add this user to the dispatch
        // if not continue
        match sub_data.subscribe_user(user_id, call.to_string()) {
            // TODO: change id
            Ok(id) => return Ok(format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}}", id)),
            Err(_) => todo!(),
        }

    } else {
        // Replace block tags if applicable
        call = replace_block_tags(&mut call, &cache_args.named_numbers);
    }

    call["id"] = user_id.into();
    incoming_tx
        .send(WsconnMessage::Message(call.clone()))
        .expect("Failed to send message to ws_conn_manager");
    let mut response = listen_for_response(user_id, broadcast_rx).await?;

    if is_subscription {
        // add the subscription id and add this user to the dispatch
        sub_data.register_subscription(call.to_string(), response["result"].as_str().unwrap().to_string(), node_id);
        sub_data.subscribe_user(user_id, subscription_id.to_string());
    } else {
        cache_querry(&mut response.to_string(), call, tx_hash, cache_args);
    }

    response["id"] = id;
    Ok(response.to_string())
}

async fn listen_for_response(
    user_id: u64,
    mut broadcast_rx: broadcast::Receiver<IncomingResponse>,
) -> Result<Value, Error> {
    while let Ok(response) = broadcast_rx.recv().await {
        if response.content["id"] == user_id {
            return Ok(response.content);
        }
    }
    Err("Failed to receive response from WS".into())
}
