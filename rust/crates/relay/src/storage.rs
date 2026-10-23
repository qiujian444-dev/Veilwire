//! 中继持久化：追加日志 + 启动重放
//!
//! 为什么不直接用 SQLite：
//! - 这台机器网络不稳，新增 C 依赖（rusqlite bundled）要重新拉包，容易卡住；
//! - 中继的写入模式极简单：只有"建队列 / 投递 / 确认"三种事件，纯追加；
//! - 追加日志的逻辑可以完全单元测试（重放是确定性的），出问题好排查。
//!
//! 权衡说明（必须在部署文档里写清楚）：
//! - 默认**不做 fsync**：崩溃最多丢最后几秒的写入。对 IM 可接受——客户端没收到 ACK 会重发。
//!   加 `--fsync` 可以每条都落盘，代价是吞吐掉一个量级。
//! - 日志会随 ack 增长，靠 `compact` 回收。
//! - 单行损坏（断电写坏）**跳过而不是整体失败**，否则一次掉电就让中继永远起不来。
//!
//! 生产化时应该换成"独立写线程 + 预写日志 + 定期快照"，这里先保证正确性。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::{Queue, StoredMsg};

/// 一条持久化记录
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Record {
    /// 建队列。`exp` 是绝对时间戳（unix 秒）
    New { qid: String, exp: u64 },
    /// 投递。`body` 用 base64，比 JSON 数字数组省 3 倍空间
    Put {
        qid: String,
        id: String,
        body: String,
        ts: u64,
    },
    /// 确认消费
    Ack { qid: String, ids: Vec<String> },
}

pub struct Log {
    path: PathBuf,
    file: Option<File>,
    /// 本次启动后追加的字节数，用于判断是否需要 compact
    appended: u64,
    fsync: bool,
}

/// 重放结果
pub struct Loaded {
    pub queues: HashMap<String, Queue>,
    /// 跳过的损坏行数（>0 说明上次可能异常退出）
    pub corrupted_lines: usize,
}

impl Log {
    /// 打开（或创建）数据目录，重放已有日志。
    pub fn open(dir: &Path, fsync: bool) -> Result<(Self, Loaded)> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("无法创建数据目录 {}", dir.display()))?;
        let path = dir.join("queues.log");

        let (queues, corrupted_lines) = if path.exists() {
            replay(&path)?
        } else {
            (HashMap::new(), 0)
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("无法打开日志 {}", path.display()))?;

        Ok((
            Self {
                path,
                file: Some(file),
                appended: 0,
                fsync,
            },
            Loaded {
                queues,
                corrupted_lines,
            },
        ))
    }

    /// 纯内存模式（测试用）
    pub fn memory() -> Self {
        Self {
            path: PathBuf::new(),
            file: None,
            appended: 0,
            fsync: false,
        }
    }

    pub fn is_persistent(&self) -> bool {
        self.file.is_some()
    }

    pub fn append_new(&mut self, qid: &str, exp: u64) -> Result<()> {
        self.write(&Record::New {
            qid: qid.to_string(),
            exp,
        })
    }

    pub fn append_put(&mut self, qid: &str, id: &str, body: &[u8], ts: u64) -> Result<()> {
        self.write(&Record::Put {
            qid: qid.to_string(),
            id: id.to_string(),
            body: STANDARD_NO_PAD.encode(body),
            ts,
        })
    }

    pub fn append_ack(&mut self, qid: &str, ids: &[String]) -> Result<()> {
        self.write(&Record::Ack {
            qid: qid.to_string(),
            ids: ids.to_vec(),
        })
    }

    fn write(&mut self, rec: &Record) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(()); // 内存模式
        };
        let mut line = serde_json::to_string(rec).context("记录序列化失败")?;
        line.push('\n');
        file.write_all(line.as_bytes()).context("写入日志失败")?;
        if self.fsync {
            file.sync_data().context("fsync 失败")?;
        }
        self.appended += line.len() as u64;
        Ok(())
    }

    /// 是否值得压缩：本次追加量超过"有效数据量"的两倍，说明里面大多是已 ack 的垃圾。
    pub fn should_compact(&self, queues: &HashMap<String, Queue>) -> bool {
        if self.file.is_none() {
            return false;
        }
        let live = live_entry_count(queues) as u64;
        // 阈值：至少 64KB 才考虑，避免小规模时频繁重写
        self.appended > (live * 120).max(64 * 1024)
    }

    /// 把当前内存状态重写成新日志，然后原子替换。
    ///
    /// 为什么先写临时文件再 rename：rename 在同一文件系统上是原子的，
    /// 中途崩溃要么是旧文件、要么是新文件，不会出现半截日志。
    pub fn compact(&mut self, queues: &HashMap<String, Queue>) -> Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        // 先关掉句柄，否则 Windows 上 rename 会失败
        self.file = None;

        let tmp = self.path.with_extension("log.tmp");
        {
            let mut f = File::create(&tmp).context("创建临时日志失败")?;
            for (qid, q) in queues {
                let line = serde_json::to_string(&Record::New {
                    qid: qid.clone(),
                    exp: q.expires_at,
                })?;
                writeln!(f, "{line}")?;
                for m in &q.msgs {
                    let line = serde_json::to_string(&Record::Put {
                        qid: qid.clone(),
                        id: m.id.clone(),
                        body: STANDARD_NO_PAD.encode(&m.body),
                        ts: m.ts,
                    })?;
                    writeln!(f, "{line}")?;
                }
            }
            f.sync_all().context("临时日志落盘失败")?;
        }

        std::fs::rename(&tmp, &self.path).context("替换日志失败")?;

        self.file = Some(OpenOptions::new().append(true).open(&self.path)?);
        self.appended = 0;
        Ok(())
    }
}

/// 有效记录数 = 队列数 + 未 ack 的消息数
fn live_entry_count(queues: &HashMap<String, Queue>) -> usize {
    queues.len() + queues.values().map(|q| q.msgs.len()).sum::<usize>()
}

fn replay(path: &Path) -> Result<(HashMap<String, Queue>, usize)> {
    let f = File::open(path).with_context(|| format!("打开日志失败 {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut queues: HashMap<String, Queue> = HashMap::new();
    let mut corrupted = 0usize;

    for (idx, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                corrupted += 1;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let rec: Record = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => {
                corrupted += 1;
                eprintln!("[storage] 第 {} 行损坏，已跳过", idx + 1);
                continue;
            }
        };

        match rec {
            Record::New { qid, exp } => {
                queues.insert(
                    qid,
                    Queue {
                        msgs: Vec::new(),
                        expires_at: exp,
                    },
                );
            }
            Record::Put {
                qid,
                id,
                body,
                ts,
            } => {
                let Ok(bytes) = STANDARD_NO_PAD.decode(&body) else {
                    corrupted += 1;
                    continue;
                };
                // 队列可能已过期被删——容忍，数据本来也要丢
                if let Some(q) = queues.get_mut(&qid) {
                    if !q.msgs.iter().any(|m| m.id == id) {
                        q.msgs.push(StoredMsg {
                            id,
                            body: bytes,
                            ts,
                        });
                    }
                }
            }
            Record::Ack { qid, ids } => {
                if let Some(q) = queues.get_mut(&qid) {
                    q.msgs.retain(|m| !ids.contains(&m.id));
                }
            }
        }
    }

    Ok((queues, corrupted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "relay-store-{}-{}-{}",
            tag,
            std::process::id(),
            proto::new_queue_id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn q(exp: u64) -> Queue {
        Queue {
            msgs: Vec::new(),
            expires_at: exp,
        }
    }

    #[test]
    fn replay_restores_queues_and_messages() {
        let dir = tmpdir("restore");
        {
            let (mut log, loaded) = Log::open(&dir, false).unwrap();
            assert!(loaded.queues.is_empty());
            log.append_new("aaaa", 2_000_000_000).unwrap();
            log.append_put("aaaa", "m1", b"hello", 100).unwrap();
            log.append_put("aaaa", "m2", b"world", 101).unwrap();
        }

        let (_log, loaded) = Log::open(&dir, false).unwrap();
        let q = loaded.queues.get("aaaa").expect("队列应被恢复");
        assert_eq!(q.msgs.len(), 2);
        assert_eq!(q.msgs[0].body, b"hello");
        assert_eq!(q.msgs[1].id, "m2");
        assert_eq!(loaded.corrupted_lines, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ack_removes_message_after_reload() {
        let dir = tmpdir("ack");
        {
            let (mut log, _) = Log::open(&dir, false).unwrap();
            log.append_new("bbbb", 2_000_000_000).unwrap();
            log.append_put("bbbb", "m1", b"x", 1).unwrap();
            log.append_put("bbbb", "m2", b"y", 2).unwrap();
            log.append_ack("bbbb", &["m1".to_string()]).unwrap();
        }
        let (_log, loaded) = Log::open(&dir, false).unwrap();
        let q = loaded.queues.get("bbbb").unwrap();
        assert_eq!(q.msgs.len(), 1, "被 ack 的消息不该再出现");
        assert_eq!(q.msgs[0].id, "m2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compact_keeps_state_and_shrinks_file() {
        let dir = tmpdir("compact");
        let mut queues: HashMap<String, Queue> = HashMap::new();
        queues.insert("cccc".into(), q(2_000_000_000));

        let (mut log, _) = Log::open(&dir, false).unwrap();
        log.append_new("cccc", 2_000_000_000).unwrap();
        // 写足够多的记录再全部 ack，制造大量垃圾。
        // 注意要跨过 should_compact 的最小阈值（64KB），否则测不到压缩路径。
        for i in 0..700 {
            let id = format!("m{i}");
            log.append_put("cccc", &id, b"payload-payload", i).unwrap();
            queues.get_mut("cccc").unwrap().msgs.push(StoredMsg {
                id: id.clone(),
                body: b"payload-payload".to_vec(),
                ts: i,
            });
            log.append_ack("cccc", &[id]).unwrap();
            queues.get_mut("cccc").unwrap().msgs.clear();
        }
        let before = std::fs::metadata(dir.join("queues.log")).unwrap().len();
        assert!(
            before > 64 * 1024,
            "测试数据要足够大才会触发压缩，当前只有 {before} 字节"
        );
        assert!(log.should_compact(&queues));

        log.compact(&queues).unwrap();
        let after = std::fs::metadata(dir.join("queues.log")).unwrap().len();
        assert!(after < before, "压缩后应变小: {before} -> {after}");

        // 压缩后重放状态必须一致
        drop(log);
        let (_l, loaded) = Log::open(&dir, false).unwrap();
        assert!(loaded.queues.contains_key("cccc"));
        assert!(loaded.queues["cccc"].msgs.is_empty(), "已 ack 的消息不该复活");
        assert_eq!(loaded.corrupted_lines, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupted_line_is_skipped_not_fatal() {
        // 模拟断电写坏一行：必须能启动，而不是永远起不来
        let dir = tmpdir("corrupt");
        {
            let (mut log, _) = Log::open(&dir, false).unwrap();
            log.append_new("dddd", 2_000_000_000).unwrap();
            log.append_put("dddd", "m1", b"ok", 1).unwrap();
        }
        // 手动塞一行垃圾
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(dir.join("queues.log"))
                .unwrap();
            writeln!(f, "{{ this is not json").unwrap();
        }

        let (_log, loaded) = Log::open(&dir, false).unwrap();
        assert_eq!(loaded.corrupted_lines, 1);
        assert_eq!(loaded.queues["dddd"].msgs.len(), 1, "好数据必须保住");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_put_not_duplicated_on_replay() {
        let dir = tmpdir("dup");
        {
            let (mut log, _) = Log::open(&dir, false).unwrap();
            log.append_new("eeee", 2_000_000_000).unwrap();
            log.append_put("eeee", "same", b"x", 1).unwrap();
            log.append_put("eeee", "same", b"x", 1).unwrap();
        }
        let (_log, loaded) = Log::open(&dir, false).unwrap();
        assert_eq!(loaded.queues["eeee"].msgs.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn put_to_unknown_queue_is_ignored_on_replay() {
        // 队列过期被删后，迟到的 put 不该让重放失败
        let dir = tmpdir("orphan");
        {
            let (mut log, _) = Log::open(&dir, false).unwrap();
            log.append_put("ffff", "m1", b"orphan", 1).unwrap();
        }
        let (_log, loaded) = Log::open(&dir, false).unwrap();
        assert!(loaded.queues.is_empty());
        assert_eq!(loaded.corrupted_lines, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_mode_is_noop() {
        let mut log = Log::memory();
        assert!(!log.is_persistent());
        // 不应 panic，也不应报错
        log.append_new("x", 1).unwrap();
        log.append_put("x", "m", b"b", 1).unwrap();
        log.append_ack("x", &[]).unwrap();
    }
}
