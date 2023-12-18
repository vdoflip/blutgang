use crate::{
    balancer::{
        format::replace_block_tags,
        processing::{
            cache_querry,
            CacheArgs,
        },
        selection::select::pick,
    },
    rpc::types::{
        hex_to_decimal,
        Rpc,
    },
    websocket::{
        subscription_manager::insert_and_return_subscription,
        types::{
            SubscriptionData,
            WsChannelErr,
            WsconnMessage,
        },
    },
};

use std::sync::{
    Arc,
    RwLock,
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

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

pub async fn ws_conn_manager(
    rpc_list: Arc<RwLock<Vec<Rpc>>>,
    mut incoming_rx: mpsc::UnboundedReceiver<WsconnMessage>,
    broadcast_tx: broadcast::Sender<Value>,
    ws_error_tx: mpsc::UnboundedSender<WsChannelErr>,
) {
    let mut ws_handles = create_ws_vec(&rpc_list, &broadcast_tx, ws_error_tx.clone()).await;

    while let Some(message) = incoming_rx.recv().await {
        match message {
            WsconnMessage::Message(incoming) => {
                if let Some(rpc_position) = {
                    let mut rpc_list_guard = rpc_list.write().unwrap();
                    pick(&mut rpc_list_guard).1
                } {
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
                ws_handles = create_ws_vec(&rpc_list, &broadcast_tx, ws_error_tx.clone()).await;
            }
        }
    }
}

pub async fn create_ws_vec(
    rpc_list: &Arc<RwLock<Vec<Rpc>>>,
    broadcast_tx: &broadcast::Sender<Value>,
    ws_error_tx: mpsc::UnboundedSender<WsChannelErr>,
) -> Vec<Option<mpsc::UnboundedSender<Value>>> {
    let rpc_list_clone = rpc_list.read().unwrap().clone();
    let mut ws_handles = Vec::new();

    for (index, rpc) in rpc_list_clone.iter().enumerate() {
        let (ws_conn_incoming_tx, ws_conn_incoming_rx) = mpsc::unbounded_channel();
        ws_handles.push(Some(ws_conn_incoming_tx));
        ws_conn(
            rpc.clone(),
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
    mut incoming_rx: mpsc::UnboundedReceiver<Value>,
    broadcast_tx: broadcast::Sender<Value>,
    ws_error_tx: mpsc::UnboundedSender<WsChannelErr>,
    index: usize,
) {
    let url = reqwest::Url::parse(&rpc.ws_url.unwrap()).expect("Failed to parse URL");
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect to WS");
    let (mut write, mut read) = ws_stream.split();

    tokio::spawn(async move {
        while let Some(incoming) = incoming_rx.recv().await {
            if incoming["method"] == "close" {
                let _ = write.close().await;
                break;
            }
            let _ = write.send(Message::Text(incoming.to_string())).await;
        }
    });

    tokio::spawn(async move {
        while let Some(result) = read.next().await {
            match result {
                Ok(message) => {
                    let rax =
                        unsafe { simd_json::from_str(&mut message.into_text().unwrap()).unwrap() };
                    let _ = broadcast_tx.send(rax);
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
    incoming_tx: mpsc::UnboundedSender<WsconnMessage>,
    broadcast_rx: broadcast::Receiver<Value>,
    sub_data: Arc<SubscriptionData>,
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

    let is_subscription = call["method"] == "eth_subscribe";
    if is_subscription {
        // Check if we're already subscribed to this
        // if so return the subscription id and add this user to the dispatch
        // if not continue
    } else {
        call = replace_block_tags(&mut call, &cache_args.named_numbers);
    }

    call["id"] = user_id.into();
    incoming_tx
        .send(WsconnMessage::Message(call.clone()))
        .expect("Failed to send message to ws_conn_manager");
    let mut response = listen_for_response(user_id, broadcast_rx).await?;

    if is_subscription {
        // add the subscription id and add this user to the dispatch
    } else {
        cache_querry(&mut response.to_string(), call, tx_hash, cache_args);
    }

    response["id"] = id;
    Ok(response.to_string())
}

async fn listen_for_response(
    user_id: u64,
    mut broadcast_rx: broadcast::Receiver<Value>,
) -> Result<Value, Error> {
    while let Ok(response) = broadcast_rx.recv().await {
        if response["id"] == user_id {
            return Ok(response);
        }
    }
    Err("Failed to receive response from WS".into())
}
