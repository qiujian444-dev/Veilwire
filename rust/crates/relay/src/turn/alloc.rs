//! TURN 分配表：端口池、权限、通道、生命周期。
//!
//! 一台中继上同时可能有几十路通话，每一路是一个"分配"：
//! 客户端地址 → (relay 端口, 权限集合, 通道表, 到期时间)。
//! 数据面只转发"已授权对端"的包（permission 或 channel），其余丢弃。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

pub const DEFAULT_LIFETIME: u32 = 600;
pub const MIN_LIFETIME: u32 = 60;
pub const MAX_LIFETIME: u32 = 3600;

/// relay 端口池。按区间循环分配，回收后可复用。
pub struct PortPool {
    min: u16,
    max: u16,
    next: u16,
    used: HashSet<u16>,
    /// 刚释放的端口优先复用：避免长时间运行后端口号一路走高、把区间用"散"
    released: Vec<u16>,
}

impl PortPool {
    pub fn new(min: u16, max: u16) -> PortPool {
        PortPool { min, max, next: min, used: HashSet::new(), released: Vec::new() }
    }

    pub fn capacity(&self) -> usize {
        (self.max as usize).saturating_sub(self.min as usize) + 1
    }

    pub fn acquire(&mut self) -> Option<u16> {
        // 先捡释放过的
        while let Some(p) = self.released.pop() {
            if !self.used.contains(&p) {
                self.used.insert(p);
                return Some(p);
            }
        }
        for _ in 0..self.capacity() {
            let p = self.next;
            self.next = if self.next >= self.max { self.min } else { self.next + 1 };
            if !self.used.contains(&p) {
                self.used.insert(p);
                return Some(p);
            }
        }
        None
    }

    pub fn release(&mut self, port: u16) {
        if self.used.remove(&port) {
            self.released.push(port);
        }
    }
}

pub struct Alloc {
    pub client: SocketAddr,
    pub username: String,
    pub relay: Arc<UdpSocket>,
    pub port: u16,
    pub expires: Instant,
    /// 允许把数据发往这些 IP（RFC 5766 的 permission 只按 IP 授权，不按端口）
    pub perms: HashSet<IpAddr>,
    /// channel number → peer
    pub chan_to_peer: HashMap<u16, SocketAddr>,
    /// peer → channel number（回程用它判断能否走 ChannelData）
    pub peer_to_chan: HashMap<SocketAddr, u16>,
    /// 分配存活标志：删除分配时置 false，读 relay socket 的任务据此退出
    pub alive: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AllocErr {
    /// 同一 IP 分配数超限 或 端口耗尽 → 486
    Quota,
}

pub struct State {
    pub allocs: HashMap<SocketAddr, Alloc>,
    pool: PortPool,
    per_ip: HashMap<IpAddr, usize>,
    /// NAT 后的公网地址：XOR-RELAYED-ADDRESS 必须通告它，否则对端连不上
    pub external_ip: IpAddr,
    pub max_per_ip: usize,
}

impl State {
    pub fn new(external_ip: IpAddr, port_min: u16, port_max: u16, max_per_ip: usize) -> State {
        State {
            allocs: HashMap::new(),
            pool: PortPool::new(port_min, port_max),
            per_ip: HashMap::new(),
            external_ip,
            max_per_ip,
        }
    }

    pub fn exists(&self, client: &SocketAddr) -> bool {
        self.allocs.contains_key(client)
    }

    pub fn get(&self, client: &SocketAddr) -> Option<&Alloc> {
        self.allocs.get(client)
    }

    #[cfg(test)]
    pub fn get_mut(&mut self, client: &SocketAddr) -> Option<&mut Alloc> {
        self.allocs.get_mut(client)
    }

    /// 新建分配。relay socket 绑定 0.0.0.0:<port>：
    /// 这台机器上绑具体内网 IP 的 UDP socket 收不到公网包（已实测），必须绑通配。
    pub async fn allocate(
        &mut self,
        client: SocketAddr,
        username: &str,
        lifetime: u32,
    ) -> Result<u16, AllocErr> {
        let ip = client.ip();
        let n = self.per_ip.get(&ip).copied().unwrap_or(0);
        if n >= self.max_per_ip {
            return Err(AllocErr::Quota);
        }
        let port = self.pool.acquire().ok_or(AllocErr::Quota)?;
        // bind 失败换下一个端口重试：remove() 释放的端口可能还被刚退出的
        // reader 任务短暂持有（Arc<UdpSocket> 未 drop），立即复用会 EADDRINUSE。
        // 早先把 bind 失败一律映射成 Quota，叠加"released 优先弹出"，会让
        // 所有新分配在窗口内连续 486。
        let sock = {
            let mut tried = 0;
            let mut cur = Some(port);
            let mut made = None;
            while tried < 3 {
                let p = cur.take().unwrap();
                match UdpSocket::bind(("0.0.0.0", p)).await {
                    Ok(s) => {
                        made = Some((p, s));
                        break;
                    }
                    Err(_) => {
                        self.pool.release(p);
                        tried += 1;
                        cur = self.pool.acquire();
                        if cur.is_none() {
                            break;
                        }
                    }
                }
            }
            match made {
                Some((p, s)) => (p, s),
                None => return Err(AllocErr::Quota),
            }
        };
        let port = sock.0;
        let sock = sock.1;
        let secs = lifetime.clamp(MIN_LIFETIME, MAX_LIFETIME);
        self.allocs.insert(
            client,
            Alloc {
                client,
                username: username.to_string(),
                relay: Arc::new(sock),
                port,
                expires: Instant::now() + Duration::from_secs(secs as u64),
                perms: HashSet::new(),
                chan_to_peer: HashMap::new(),
                peer_to_chan: HashMap::new(),
                alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        );
        *self.per_ip.entry(ip).or_insert(0) += 1;
        Ok(port)
    }

    pub fn refresh(&mut self, client: &SocketAddr, lifetime: u32) -> Option<u32> {
        let a = self.allocs.get_mut(client)?;
        let secs = lifetime.clamp(MIN_LIFETIME, MAX_LIFETIME);
        a.expires = Instant::now() + Duration::from_secs(secs as u64);
        Some(secs)
    }

    pub fn remove(&mut self, client: &SocketAddr) -> Option<Alloc> {
        let a = self.allocs.remove(client)?;
        a.alive.store(false, std::sync::atomic::Ordering::Relaxed);
        self.pool.release(a.port);
        if let Some(n) = self.per_ip.get_mut(&a.client.ip()) {
            *n = n.saturating_sub(1);
        }
        Some(a)
    }

    pub fn add_permission(&mut self, client: &SocketAddr, peer_ip: IpAddr) -> bool {
        match self.allocs.get_mut(client) {
            Some(a) => {
                a.perms.insert(peer_ip);
                true
            }
            None => false,
        }
    }

    pub fn bind_channel(&mut self, client: &SocketAddr, num: u16, peer: SocketAddr) -> bool {
        match self.allocs.get_mut(client) {
            Some(a) => {
                // 一个通道只能绑一个对端，一个对端也只能有一个通道：先解绑旧的
                if let Some(old) = a.chan_to_peer.remove(&num) {
                    a.peer_to_chan.remove(&old);
                }
                if let Some(old_num) = a.peer_to_chan.remove(&peer) {
                    a.chan_to_peer.remove(&old_num);
                }
                a.chan_to_peer.insert(num, peer);
                a.peer_to_chan.insert(peer, num);
                // 绑通道等价于对该 IP 授权（RFC 5766 §11.2）
                a.perms.insert(peer.ip());
                true
            }
            None => false,
        }
    }

    /// 清理到期分配，返回清掉的条数
    pub fn sweep(&mut self) -> usize {
        let now = Instant::now();
        let expired: Vec<SocketAddr> = self
            .allocs
            .iter()
            .filter(|(_, a)| a.expires <= now)
            .map(|(k, _)| *k)
            .collect();
        let n = expired.len();
        for k in expired {
            self.remove(&k);
        }
        n
    }

    pub fn count(&self) -> usize {
        self.allocs.len()
    }

    pub fn free_ports(&self) -> usize {
        self.pool.capacity() - self.allocs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例用不同的端口段：单测默认并发跑，同一段端口会互相抢（UDP bind 失败）
    fn st(base: u16) -> State {
        State::new("203.0.113.1".parse().unwrap(), base, base + 3, 2)
    }

    #[test]
    fn pool_cycles_and_reuses() {
        let mut p = PortPool::new(49160, 49161);
        let a = p.acquire().unwrap();
        let b = p.acquire().unwrap();
        assert_ne!(a, b);
        assert!(p.acquire().is_none(), "两端口用尽后必须返回 None");
        p.release(a);
        assert_eq!(p.acquire(), Some(a), "释放后应能复用");
    }

    #[tokio::test]
    async fn allocate_respects_quota_and_lifetime() {
        let mut s = st(50100);
        let c1: SocketAddr = "198.51.100.7:1000".parse().unwrap();
        let c2: SocketAddr = "198.51.100.7:1001".parse().unwrap();
        let c3: SocketAddr = "198.51.100.7:1002".parse().unwrap();
        assert!(s.allocate(c1, "u", DEFAULT_LIFETIME).await.is_ok());
        assert!(s.allocate(c2, "u", DEFAULT_LIFETIME).await.is_ok());
        // 同一 IP 超过 max_per_ip=2
        assert_eq!(
            s.allocate(c3, "u", DEFAULT_LIFETIME).await.err(),
            Some(AllocErr::Quota)
        );
        // 另一个 IP 还能分配
        let other: SocketAddr = "198.51.100.8:1000".parse().unwrap();
        assert!(s.allocate(other, "u", DEFAULT_LIFETIME).await.is_ok());
    }

    #[tokio::test]
    async fn remove_returns_port_and_quota() {
        let mut s = st(50200);
        let c: SocketAddr = "198.51.100.9:1".parse().unwrap();
        let port = s.allocate(c, "u", DEFAULT_LIFETIME).await.unwrap();
        assert!(s.remove(&c).is_some());
        assert_eq!(s.count(), 0);
        // 端口回到池里，能再次分配
        let mut s2 = s;
        let c2: SocketAddr = "198.51.100.9:2".parse().unwrap();
        let port2 = s2.allocate(c2, "u", DEFAULT_LIFETIME).await.unwrap();
        assert_eq!(port, port2, "释放的端口应被优先复用");
    }

    #[tokio::test]
    async fn channel_binding_is_exclusive() {
        let mut s = st(50300);
        let c: SocketAddr = "198.51.100.10:1".parse().unwrap();
        s.allocate(c, "u", DEFAULT_LIFETIME).await.unwrap();
        let p1: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        let p2: SocketAddr = "203.0.113.6:5000".parse().unwrap();
        assert!(s.bind_channel(&c, 0x4000, p1));
        assert!(s.bind_channel(&c, 0x4000, p2), "同通道换绑对端应成功");
        let a = s.get(&c).unwrap();
        assert_eq!(a.chan_to_peer.get(&0x4000), Some(&p2));
        assert!(!a.peer_to_chan.contains_key(&p1), "旧对端映射必须清掉");
        // 绑通道同时授予了 permission
        assert!(a.perms.contains(&p2.ip()));
    }

    #[tokio::test]
    async fn sweep_drops_expired() {
        let mut s = st(50400);
        let c: SocketAddr = "198.51.100.11:1".parse().unwrap();
        s.allocate(c, "u", MIN_LIFETIME).await.unwrap();
        // 手动把到期时间拨到过去
        s.get_mut(&c).unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(s.sweep(), 1);
        assert_eq!(s.count(), 0);
    }
}
