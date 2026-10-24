//! 最小 MQTT 3.1.1 客户端（QoS0），专为 Blinker 的 MQTT broker 设计。
//!
//! 只实现需要的部分：CONNECT / CONNACK / SUBSCRIBE / SUBACK / PUBLISH / PINGREQ。
//! 走明文 1883 端口，不需要 TLS（Blinker 支持 mqtt://broker.diandeng.tech:1883）。

use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read, Write};

const CONNECT: u8 = 1;
const CONNACK: u8 = 2;
const PUBLISH: u8 = 3;
const SUBSCRIBE: u8 = 8;
const SUBACK: u8 = 9;
const PINGREQ: u8 = 12;
const PINGRESP: u8 = 13;

#[derive(Debug)]
pub enum MqttError {
    Socket,
    UnexpectedEof,
    BadPacket,
    Rejected,
    Timeout,
}

impl From<embassy_net::tcp::Error> for MqttError {
    fn from(_: embassy_net::tcp::Error) -> Self {
        MqttError::Socket
    }
}

/// 把长度编码成 MQTT 变长字节，返回写入的字节数
fn encode_len(buf: &mut [u8], mut len: usize) -> usize {
    let mut i = 0;
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        buf[i] = byte;
        i += 1;
        if len == 0 || i >= 4 {
            break;
        }
    }
    i
}

/// 写 2 字节大端字符串（"长度 + 内容"）
fn push_str(buf: &mut [u8], pos: usize, s: &str) -> usize {
    let b = s.as_bytes();
    buf[pos] = (b.len() >> 8) as u8;
    buf[pos + 1] = (b.len() & 0xFF) as u8;
    buf[pos + 2..pos + 2 + b.len()].copy_from_slice(b);
    pos + 2 + b.len()
}

pub struct MqttClient<'a, 's> {
    /// 借用而不是持有：socket 要跨重连循环复用，它的收发缓冲是 static 的，
    /// `mk_static!` 只能 write 一次，所以不能每次重连都重建一个 socket。
    socket: &'s mut TcpSocket<'a>,
    keepalive_secs: u16,
}

impl<'a, 's> MqttClient<'a, 's> {
    pub fn new(socket: &'s mut TcpSocket<'a>, keepalive_secs: u16) -> Self {
        Self {
            socket,
            keepalive_secs,
        }
    }

    /// 发送 CONNECT 并等待 CONNACK
    pub async fn connect(
        &mut self,
        client_id: &str,
        username: &str,
        password: &str,
    ) -> Result<(), MqttError> {
        let mut p = [0u8; 512];
        let mut n = 0;

        // 可变头：协议名 + 级别 + 标志 + keepalive
        n = push_str(&mut p, n, "MQTT");
        p[n] = 0x04; // MQTT 3.1.1
        n += 1;
        p[n] = 0xC2; // username | password | clean session
        n += 1;
        p[n] = (self.keepalive_secs >> 8) as u8;
        p[n + 1] = (self.keepalive_secs & 0xFF) as u8;
        n += 2;

        // 载荷：clientId / username / password
        n = push_str(&mut p, n, client_id);
        n = push_str(&mut p, n, username);
        n = push_str(&mut p, n, password);

        let mut pkt = [0u8; 520];
        pkt[0] = CONNECT << 4;
        let l = encode_len(&mut pkt[1..], n);
        pkt[1 + l..1 + l + n].copy_from_slice(&p[..n]);

        self.socket
            .write_all(&pkt[..1 + l + n])
            .await
            .map_err(|_| MqttError::Socket)?;
        self.socket.flush().await.map_err(|_| MqttError::Socket)?;

        // CONNACK: 0x20 0x02 0x00 <return code>
        let mut hdr = [0u8; 2];
        self.read_exact(&mut hdr).await?;
        if hdr[0] != (CONNACK << 4) || hdr[1] != 2 {
            return Err(MqttError::BadPacket);
        }
        let mut body = [0u8; 2];
        self.read_exact(&mut body).await?;
        if body[1] != 0 {
            return Err(MqttError::Rejected);
        }
        Ok(())
    }

    pub async fn subscribe(&mut self, topic: &str, packet_id: u16) -> Result<(), MqttError> {
        let mut p = [0u8; 512];
        p[0] = (packet_id >> 8) as u8;
        p[1] = (packet_id & 0xFF) as u8;
        let mut n = 2;
        n = push_str(&mut p, n, topic);
        p[n] = 0x00; // QoS 0
        n += 1;

        let mut pkt = [0u8; 520];
        pkt[0] = (SUBSCRIBE << 4) | 0x02;
        let l = encode_len(&mut pkt[1..], n);
        pkt[1 + l..1 + l + n].copy_from_slice(&p[..n]);

        self.socket
            .write_all(&pkt[..1 + l + n])
            .await
            .map_err(|_| MqttError::Socket)?;
        self.socket.flush().await.map_err(|_| MqttError::Socket)?;

        // SUBACK
        let mut hdr = [0u8; 2];
        self.read_exact(&mut hdr).await?;
        if hdr[0] != (SUBACK << 4) {
            return Err(MqttError::BadPacket);
        }
        // SUBACK 载荷：packet_id(2) + 每个 topic 一个返回码，最多读 16 字节
        let mut body = [0u8; 16];
        let blen = (hdr[1] as usize).min(body.len());
        self.read_exact(&mut body[..blen]).await?;
        Ok(())
    }

    pub async fn ping(&mut self) -> Result<(), MqttError> {
        self.socket
            .write_all(&[PINGREQ << 4, 0])
            .await
            .map_err(|_| MqttError::Socket)?;
        self.socket.flush().await.map_err(|_| MqttError::Socket)?;
        Ok(())
    }

    /// 发一条 QoS0 PUBLISH。Blinker 的心跳回复要用它。
    pub async fn publish(&mut self, topic: &str, payload: &[u8]) -> Result<(), MqttError> {
        let mut p = [0u8; 512];
        let mut n = push_str(&mut p, 0, topic);
        if n + payload.len() > p.len() {
            return Err(MqttError::BadPacket);
        }
        p[n..n + payload.len()].copy_from_slice(payload);
        n += payload.len();

        let mut pkt = [0u8; 520];
        pkt[0] = PUBLISH << 4; // QoS 0，没有 packet id
        let l = encode_len(&mut pkt[1..], n);
        pkt[1 + l..1 + l + n].copy_from_slice(&p[..n]);

        self.socket
            .write_all(&pkt[..1 + l + n])
            .await
            .map_err(|_| MqttError::Socket)?;
        self.socket.flush().await.map_err(|_| MqttError::Socket)?;
        Ok(())
    }

    /// 阻塞等待下一条 PUBLISH，返回其 payload（拷贝到 out，返回长度）
    /// 收到 PINGRESP 会自动跳过继续等
    pub async fn wait_publish(&mut self, out: &mut [u8]) -> Result<usize, MqttError> {
        loop {
            let mut hdr = [0u8; 1];
            self.read_exact(&mut hdr).await?;
            let ptype = hdr[0] >> 4;

            // 读剩余长度
            let mut rem: u32 = 0;
            let mut mult: u32 = 1;
            let mut b = [0u8; 1];
            loop {
                self.read_exact(&mut b).await?;
                rem += ((b[0] & 0x7F) as u32) * mult;
                if b[0] & 0x80 == 0 {
                    break;
                }
                mult *= 128;
            }

            let len = rem as usize;
            if len == 0 {
                if ptype == PINGRESP {
                    continue;
                }
                continue;
            }

            let mut buf = [0u8; 1024];
            let take = len.min(buf.len());
            self.read_exact(&mut buf[..take]).await?;
            // 丢弃超长部分的剩余字节
            if len > take {
                let mut rest = [0u8; 64];
                let mut left = len - take;
                while left > 0 {
                    let n = left.min(rest.len());
                    self.read_exact(&mut rest[..n]).await?;
                    left -= n;
                }
            }

            if ptype != PUBLISH {
                continue;
            }

            // 解析：topic(len+bytes) + payload
            if take < 2 {
                return Err(MqttError::BadPacket);
            }
            let tlen = ((buf[0] as usize) << 8) | buf[1] as usize;
            let payload_start = 2 + tlen;
            if payload_start > take {
                return Err(MqttError::BadPacket);
            }
            let plen = take - payload_start;
            let copy = plen.min(out.len());
            out[..copy].copy_from_slice(&buf[payload_start..payload_start + copy]);
            return Ok(copy);
        }
    }

    pub fn close(&mut self) {
        self.socket.close();
    }

    /// 带超时的读满，避免拔网线后永久卡死
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), MqttError> {
        match with_timeout(Duration::from_secs(90), self.socket.read_exact(buf)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(MqttError::Socket),
            Err(_) => Err(MqttError::Timeout),
        }
    }

    pub fn keepalive(&self) -> u16 {
        self.keepalive_secs
    }
}
