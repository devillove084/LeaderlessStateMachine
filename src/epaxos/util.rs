use std::sync::Arc;

use serde::{de::DeserializeOwned, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};

use super::instance::SharedInstance;

pub async fn instance_exist(instance: &Option<SharedInstance>) -> bool {
    if instance.is_some() {
        let ins = instance.as_ref().unwrap();
        let ins_read = ins.get_instance_read().await;
        ins_read.is_some()
    } else {
        false
    }
}

async fn read_from_stream(stream: &mut TcpStream, buf: &mut [u8]) {
    let expected_len = buf.len();
    let mut has_read = 0;
    while has_read != expected_len {
        let read_size = stream
            .read(&mut buf[has_read..])
            .await
            .map_err(|e| panic!("link should read {expected_len} bytes message, {e}"))
            .unwrap();
        has_read += read_size;
    }
}

pub async fn recv_message<M: DeserializeOwned>(conn: &mut TcpStream) -> M {
    let mut len_buf: [u8; 8] = [0; 8];
    read_from_stream(conn, &mut len_buf).await;

    let expected_len = u64::from_be_bytes(len_buf);
    let mut buf = Vec::with_capacity(expected_len as usize);
    buf.spare_capacity_mut();
    unsafe { buf.set_len(expected_len as usize) };

    read_from_stream(conn, &mut buf).await;
    bincode::deserialize(&buf)
        .map_err(|e| panic!("Deserialize message failed, {e}"))
        .unwrap()
}

pub async fn send_message<M>(conn: &mut TcpStream, message: &M)
where
    M: Serialize,
{
    // TODO: Report message content while meeting error
    let content = bincode::serialize(message)
        .map_err(|e| panic!("Failed to serialize the message, {e}"))
        .unwrap();
    let len = (content.len() as u64).to_be_bytes();

    // TODO: Handle network error
    let _ = conn.write(&len).await;
    let _ = conn.write(&content).await;
}

pub async fn send_message_arc<M>(conn: &Arc<Mutex<TcpStream>>, message: &M)
where
    M: Serialize,
{
    let mut conn = conn.lock().await;
    let conn = &mut *conn;

    send_message(conn, message).await;
}

pub async fn send_message_arc2<M>(conn: &Arc<Mutex<TcpStream>>, message: &Arc<M>)
where
    M: Serialize,
{
    send_message_arc(conn, message.as_ref()).await;
}
