//! 分片表（DESIGN.md §3.3 / §3.4）。
//!
//! 记账粒度是 `(offset, len, owner)`，持久化粒度是 1 MiB 位图 —— **两者解耦**。
//! 上一版（aria2）把两者绑死在 piece 索引上，导致抢占只能整片放弃；这里不复制那个结构。
//!
//! M1 只用单分片（`SegmentTable::single`），但类型从第一天就按 M2 的形状定义好，
//! 免得 M2 改结构体时连带改控制文件格式。

use serde::{Deserialize, Serialize};

/// 分片状态。`Active` 带 `token` 用于丢弃迟到事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegState {
    /// 还没开始，或被抢占后回到这里
    Missing,
    /// 已派发给 worker，正在等响应头或正在收数据
    Active { owner: u32, token: u64 },
    /// 已发出 Stop，等 worker 确认停止。**不能直接回 Missing**
    Reclaiming { owner: u32, token: u64 },
    Done,
}

impl SegState {
    pub fn is_done(&self) -> bool {
        matches!(self, SegState::Done)
    }

    pub fn is_active(&self) -> bool {
        matches!(self, SegState::Active { .. })
    }

    /// 取出 `Active` / `Reclaiming` 携带的 token（其它状态为 `None`）。
    ///
    /// 用这一个方法做时效校验，避免各处重复匹配两个变体。
    /// **`Reclaiming` 也必须认 token**：worker 可能在收到 Stop 的同时刚好写完，
    /// 此时它会发 `Done`（而不是 `Stopped`），记账要能接受。
    pub fn token(&self) -> Option<u64> {
        match self {
            SegState::Active { token, .. } | SegState::Reclaiming { token, .. } => Some(*token),
            _ => None,
        }
    }

    /// 是否是"派发给了这个 owner/token"的活分片。
    pub fn owned_by(&self, owner: u32, token: u64) -> bool {
        matches!(
            self,
            SegState::Active { owner: o, token: t } | SegState::Reclaiming { owner: o, token: t }
                if *o == owner && *t == token
        )
    }
}

/// 一个分片：闭区间 `[start, end]`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub state: SegState,
    /// 已写入的字节数（用于僵死检测和进度统计）
    pub written: u64,
    /// 被抢占的次数，超过上限就不再抢（防乒乓）
    pub preempts: u8,
    /// 失败重试次数，超过上限就放弃（防死循环重试）
    pub attempts: u8,
}

impl Segment {
    pub fn new(start: u64, end: u64) -> Self {
        debug_assert!(end >= start, "分片区间非法: {start}..{end}");
        Self {
            start,
            end,
            state: SegState::Missing,
            written: 0,
            preempts: 0,
            attempts: 0,
        }
    }

    /// 区间长度（闭区间，所以 +1）。
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    pub fn is_empty(&self) -> bool {
        false // 构造时已保证 end >= start
    }
}

/// 分片表。**只有调度器能改它**（单写者，DESIGN.md §3.4）。
#[derive(Debug, Clone)]
pub struct SegmentTable {
    pub segments: Vec<Segment>,
    /// 整个文件的总长
    pub total: u64,
}

impl SegmentTable {
    /// M1：整文件一个分片。
    pub fn single(total: u64) -> Self {
        if total == 0 {
            return Self {
                segments: Vec::new(),
                total: 0,
            };
        }
        Self {
            segments: vec![Segment::new(0, total - 1)],
            total,
        }
    }

    /// M2：按 `n` 切分。最后一段吃掉余数（`split_segments` 保证不丢字节）。
    pub fn split(total: u64, n: usize) -> Self {
        let segs = crate::path::split_segments(total, n)
            .into_iter()
            .map(|(s, e)| Segment::new(s, e))
            .collect();
        Self {
            segments: segs,
            total,
        }
    }

    /// 已完成字节数。
    pub fn completed(&self) -> u64 {
        self.segments
            .iter()
            .map(|s| match s.state {
                SegState::Done => s.len(),
                _ => s.written.min(s.len()),
            })
            .sum()
    }

    /// 是否全部完成。
    pub fn is_done(&self) -> bool {
        !self.segments.is_empty() && self.segments.iter().all(|s| s.state.is_done())
    }

    /// 下一个可派发的分片下标（`Missing` 里最靠前的）。
    pub fn next_missing(&self) -> Option<usize> {
        self.segments
            .iter()
            .position(|s| matches!(s.state, SegState::Missing))
    }

    /// M1：取一段**连续**的 Missing 区间，返回 `(起始下标, 数量)`。
    ///
    /// 单连接下载时把连续缺失的 piece 合并成一次 Range 请求，
    /// 但完成度仍按 piece 记账 —— 这样中断后能续，且不会退化成
    /// "3 GB 文件发 3000 个请求"。
    pub fn next_run(&self) -> Option<(usize, usize)> {
        let start = self.next_missing()?;
        let mut count = 1;
        while start + count < self.segments.len()
            && matches!(self.segments[start + count].state, SegState::Missing)
        {
            count += 1;
        }
        Some((start, count))
    }

    /// M2：取一段**连续** Missing 区间，但最多 `max` 个 piece。
    ///
    /// 并发下载时每个 worker 领一段，配合 `assign_run` 把它们标成 Active，
    /// 下一次 `next_run_limited` 自然从后面继续 —— 天然得到互不相交的多段。
    /// `max` 至少按 1 处理。
    pub fn next_run_limited(&self, max: usize) -> Option<(usize, usize)> {
        let start = self.next_missing()?;
        let max = max.max(1);
        let mut count = 1;
        while count < max
            && start + count < self.segments.len()
            && matches!(self.segments[start + count].state, SegState::Missing)
        {
            count += 1;
        }
        Some((start, count))
    }

    /// 剩余未完成字节数。
    pub fn remaining_bytes(&self) -> u64 {
        self.total.saturating_sub(self.completed())
    }

    /// 某分片被抢占的累计次数（用于僵死抢占的防乒乓上限）。
    pub fn preempts_at(&self, idx: usize) -> u8 {
        self.segments.get(idx).map(|s| s.preempts).unwrap_or(0)
    }

    /// M2：整段抢占第一阶段 —— 把该段里属于 `owner/token` 的 `Active` piece
    /// 全部转 `Reclaiming`，并累加 preempts。返回是否有 piece 被标记。
    ///
    /// 已经 `Done` 的 piece 不动（写完了就保留，抢占只放弃没写完的）。
    pub fn begin_reclaim_run(&mut self, start: usize, count: usize, owner: u32, token: u64) -> bool {
        let mut any = false;
        for i in start..(start + count).min(self.segments.len()) {
            if matches!(self.segments[i].state,
                SegState::Active { owner: o, token: t } if o == owner && t == token)
            {
                self.segments[i].state = SegState::Reclaiming { owner, token };
                self.segments[i].preempts = self.segments[i].preempts.saturating_add(1);
                any = true;
            }
        }
        any
    }

    /// M2：整段抢占第二阶段 —— 收到 worker 的 `Stopped` 后，把仍是 `Reclaiming`
    /// 且 token 匹配的 piece 退回 `Missing`（整片重下）。已 `Done` 的保留。
    pub fn finish_reclaim_run(&mut self, start: usize, count: usize, token: u64) -> bool {
        let mut any = false;
        for i in start..(start + count).min(self.segments.len()) {
            if matches!(self.segments[i].state, SegState::Reclaiming { token: t, .. } if t == token)
            {
                self.segments[i].state = SegState::Missing;
                self.segments[i].written = 0;
                any = true;
            }
        }
        any
    }

    /// M2：把该段里属于 `token` 的 piece 的重试计数 +1，返回段内最大重试次数。
    ///
    /// 调度器在收到 `Failed` 后先调它，再决定是重试还是放弃。
    pub fn bump_attempts_run(&mut self, start: usize, count: usize, token: u64) -> u8 {
        let mut mx = 0u8;
        for i in start..(start + count).min(self.segments.len()) {
            if self.segments[i].state.token() == Some(token) {
                self.segments[i].attempts = self.segments[i].attempts.saturating_add(1);
                mx = mx.max(self.segments[i].attempts);
            }
        }
        mx
    }

    /// M2：把卡在 `Reclaiming` 的孤儿 piece 强制退回 `Missing`。
    ///
    /// 只在"没有任何 worker 在跑、但仍有 Reclaiming piece"这种异常态下调用
    /// （正常流程 worker 一定会回 `Stopped`）。返回复原了几片。
    pub fn reset_reclaiming(&mut self) -> usize {
        let mut n = 0;
        for s in &mut self.segments {
            if matches!(s.state, SegState::Reclaiming { .. }) {
                s.state = SegState::Missing;
                s.written = 0;
                n += 1;
            }
        }
        n
    }

    /// 把一段连续区间整体派发给同一个 owner/token。
    pub fn assign_run(&mut self, start: usize, count: usize, owner: u32, token: u64) -> bool {
        if start + count > self.segments.len() || count == 0 {
            return false;
        }
        for i in start..start + count {
            if !matches!(self.segments[i].state, SegState::Missing) {
                return false;
            }
        }
        for i in start..start + count {
            self.segments[i].state = SegState::Active { owner, token };
            self.segments[i].written = 0;
        }
        true
    }

    /// 上报一段区间的进度：`written` 是**从区间起点算起**已写的字节数。
    ///
    /// 写满的 piece 立刻转 `Done`（这是断点续传的记账点），
    /// 正在写的那个 piece 记 `written` 但仍保持原状态。
    /// `Active` 与 `Reclaiming` 都接受（token 匹配即可）—— 见 `SegState::token`。
    /// 返回这一段里已确认完成的字节数。
    pub fn report_run_progress(
        &mut self,
        start: usize,
        count: usize,
        token: u64,
        written: u64,
    ) -> u64 {
        let mut acc = 0u64;
        let mut done_bytes = 0u64;
        for i in start..(start + count).min(self.segments.len()) {
            let piece_len = self.segments[i].len();
            let piece_end_off = acc + piece_len; // 该 piece 结束时的累计偏移
            if self.segments[i].state.token() == Some(token) {
                if written >= piece_end_off {
                    self.segments[i].state = SegState::Done;
                    self.segments[i].written = piece_len;
                    done_bytes += piece_len;
                } else {
                    self.segments[i].written = written.saturating_sub(acc);
                    break; // 后面的 piece 还没开始
                }
            } else if self.segments[i].state.is_done() {
                // 已完成的不重算
            } else {
                break;
            }
            acc = piece_end_off;
        }
        done_bytes
    }

    /// 区间下载结束（失败/放弃）：把没写满的 piece 退回 `Missing`（整片重下），
    /// 已满的保持 `Done`。返回是否有未完成的 piece 需要重来。
    ///
    /// `Active` 与 `Reclaiming` 都退回 `Missing`（token 匹配）。
    pub fn finish_run(&mut self, start: usize, count: usize, token: u64) -> bool {
        let mut any_incomplete = false;
        for i in start..(start + count).min(self.segments.len()) {
            if self.segments[i].state.token() == Some(token) {
                self.segments[i].state = SegState::Missing;
                self.segments[i].written = 0;
                any_incomplete = true;
            }
        }
        any_incomplete
    }

    /// 派发：`Missing → Active`，分配新 token。
    pub fn assign(&mut self, idx: usize, owner: u32, token: u64) -> bool {
        match self.segments.get_mut(idx) {
            Some(s) if matches!(s.state, SegState::Missing) => {
                s.state = SegState::Active { owner, token };
                s.written = 0;
                true
            }
            _ => false,
        }
    }

    /// 抢占第一阶段：`Active → Reclaiming`。**已经发 Stop 了，等 worker 确认**。
    ///
    /// 返回 `false` 表示 token 不匹配（迟到调用）或状态不对。
    pub fn begin_reclaim(&mut self, idx: usize, owner: u32, token: u64) -> bool {
        match self.segments.get_mut(idx) {
            Some(s) => match s.state {
                SegState::Active { owner: o, token: t } if o == owner && t == token => {
                    s.state = SegState::Reclaiming { owner, token };
                    s.preempts = s.preempts.saturating_add(1);
                    true
                }
                _ => false,
            },
            None => false,
        }
    }

    /// 抢占第二阶段：`Reclaiming → Missing`。只在收到 `Stopped` 后调用。
    pub fn finish_reclaim(&mut self, idx: usize, token: u64) -> bool {
        match self.segments.get_mut(idx) {
            Some(s) => match s.state {
                SegState::Reclaiming { token: t, .. } if t == token => {
                    s.state = SegState::Missing;
                    s.written = 0; // 已写字节留在文件里，但整片重下
                    true
                }
                _ => false,
            },
            None => false,
        }
    }

    /// worker 上报进度。token 不匹配 → 迟到事件，**丢弃**。
    pub fn report_progress(&mut self, idx: usize, token: u64, written: u64) -> bool {
        match self.segments.get_mut(idx) {
            Some(s) => match s.state {
                SegState::Active { token: t, .. } if t == token => {
                    s.written = written;
                    true
                }
                _ => false,
            },
            None => false,
        }
    }

    /// worker 完成。token 不匹配 → 丢弃。
    pub fn report_done(&mut self, idx: usize, token: u64) -> bool {
        match self.segments.get_mut(idx) {
            Some(s) => match s.state {
                SegState::Active { token: t, .. } if t == token => {
                    s.state = SegState::Done;
                    s.written = s.len();
                    true
                }
                _ => false,
            },
            None => false,
        }
    }

    /// 把区间落成 1 MiB 位图（用于控制文件持久化）。
    ///
    /// 只有 `Done` 的分片才置位；`written` 部分不算完成 —— 续传时重下整片，
    /// 这与 DESIGN.md §3.4 的"已写字节留文件但整片重下"一致。
    pub fn to_bitfield(&self, piece_length: u64) -> Vec<u8> {
        if piece_length == 0 || self.total == 0 {
            return Vec::new();
        }
        let pieces = self.total.div_ceil(piece_length) as usize;
        let mut bits = vec![0u8; pieces.div_ceil(8)];
        for seg in &self.segments {
            if !seg.state.is_done() {
                continue;
            }
            let first = (seg.start / piece_length) as usize;
            let last = (seg.end / piece_length) as usize;
            for p in first..=last {
                bits[p / 8] |= 1 << (p % 8);
            }
        }
        bits
    }

    /// 从位图恢复分片表（续传）。
    ///
    /// 位图的 piece 粒度与控制文件里存的 `piece_length` 绑定：
    /// 换了 piece_length，同一个 bit 就对应不同区间 → 文件静默损坏。
    /// 所以这里必须由调用方保证 `piece_length` 与文件里记录的一致。
    ///
    /// **必须保持 piece 粒度，不能把连续位合并成大段。**
    /// `report_run_progress` 是把每个 `Segment` 当作一个 piece 记账的
    /// （`piece_len = seg.len()`，写满才置 Done）。一旦合并成 3 GB 的大段，
    /// "写满"判据就永远不成立 —— 进度条会涨（`completed()` 累加 `written`），
    /// 但位图一个 bit 都不置，下次续传又从零开始。这个 bug 实测踩过：
    /// 续传后进度条走到 166.8 MB，控制文件里仍只有 92 MiB。
    ///
    /// 请求级别的合并由 `next_run()` 负责，与这里的记账粒度无关。
    pub fn from_bitfield(total: u64, piece_length: u64, bits: &[u8]) -> Self {
        if total == 0 || piece_length == 0 {
            return Self {
                segments: Vec::new(),
                total: 0,
            };
        }
        let pieces = total.div_ceil(piece_length) as usize;
        let mut segments: Vec<Segment> = Vec::with_capacity(pieces);
        for p in 0..pieces {
            let done = bits
                .get(p / 8)
                .map(|b| b & (1 << (p % 8)) != 0)
                .unwrap_or(false);
            let piece_start = p as u64 * piece_length;
            let piece_end = (piece_start + piece_length - 1).min(total - 1);
            segments.push(Self::run_segment(piece_start, piece_end, done));
        }
        Self { segments, total }
    }

    fn run_segment(start: u64, end: u64, done: bool) -> Segment {
        Segment {
            start,
            end,
            state: if done {
                SegState::Done
            } else {
                SegState::Missing
            },
            written: if done { end - start + 1 } else { 0 },
            preempts: 0,
            attempts: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_segment_covers_file() {
        let t = SegmentTable::single(1000);
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.segments[0].start, 0);
        assert_eq!(t.segments[0].end, 999);
        assert_eq!(t.segments[0].len(), 1000);
        assert_eq!(t.completed(), 0);
        assert!(!t.is_done());
    }

    #[test]
    fn zero_length_is_empty() {
        let t = SegmentTable::single(0);
        assert!(t.segments.is_empty());
        assert!(!t.is_done()); // 空表不算完成
    }

    #[test]
    fn assign_only_from_missing() {
        let mut t = SegmentTable::single(100);
        assert!(t.assign(0, 1, 42));
        assert!(!t.assign(0, 2, 43)); // 已经 Active
        assert_eq!(t.segments[0].state, SegState::Active { owner: 1, token: 42 });
    }

    #[test]
    fn stale_token_is_dropped() {
        let mut t = SegmentTable::single(100);
        t.assign(0, 1, 42);
        // 旧 token 的进度上报必须被丢弃
        assert!(!t.report_progress(0, 99, 50));
        assert!(!t.report_done(0, 99));
        // 正确 token 才生效
        assert!(t.report_progress(0, 42, 50));
        assert_eq!(t.segments[0].written, 50);
        assert!(t.report_done(0, 42));
        assert!(t.is_done());
        assert_eq!(t.completed(), 100);
    }

    #[test]
    fn two_phase_reclaim() {
        let mut t = SegmentTable::single(100);
        t.assign(0, 1, 42);

        // 第一阶段：Active → Reclaiming（已发 Stop）
        assert!(t.begin_reclaim(0, 1, 42));
        assert_eq!(t.segments[0].state, SegState::Reclaiming { owner: 1, token: 42 });
        assert_eq!(t.segments[0].preempts, 1);

        // 此时不能重新派发 —— worker 可能还在写同一区间
        assert!(!t.assign(0, 2, 43));

        // 第二阶段：收到 Stopped 才回 Missing
        assert!(t.finish_reclaim(0, 42));
        assert_eq!(t.segments[0].state, SegState::Missing);
        assert_eq!(t.segments[0].written, 0);

        // 现在可以重新派发
        assert!(t.assign(0, 2, 43));
    }

    #[test]
    fn reclaim_requires_matching_token() {
        let mut t = SegmentTable::single(100);
        t.assign(0, 1, 42);
        assert!(!t.begin_reclaim(0, 1, 99)); // token 不对
        assert!(!t.begin_reclaim(0, 2, 42)); // owner 不对
        assert!(!t.finish_reclaim(0, 99)); // 不在 Reclaiming
    }

    #[test]
    fn bitfield_roundtrip_all_done() {
        let total = 5 * 1024 * 1024 + 123;
        let pl = 1024 * 1024;
        let mut t = SegmentTable::single(total);
        t.assign(0, 1, 1);
        t.report_done(0, 1);

        let bits = t.to_bitfield(pl);
        let back = SegmentTable::from_bitfield(total, pl, &bits);
        assert!(back.is_done());
        assert_eq!(back.completed(), total);
    }

    #[test]
    fn bitfield_roundtrip_none_done() {
        let total = 5 * 1024 * 1024;
        let pl = 1024 * 1024;
        let t = SegmentTable::single(total);
        let bits = t.to_bitfield(pl);
        let back = SegmentTable::from_bitfield(total, pl, &bits);
        assert!(!back.is_done());
        assert_eq!(back.completed(), 0);

        // **必须是 piece 粒度，不能合并成一个大 Missing 段。**
        // `report_run_progress` 把每个 Segment 当一个 piece 记账，
        // 合并后"写满"判据永不成立 → 位图永不置位 → 续传失效。
        // 请求级的合并由 next_run() 负责，与记账粒度无关。
        assert_eq!(back.segments.len(), 5);
        assert_eq!(back.segments[0].start, 0);
        assert_eq!(back.segments[4].end, total - 1);

        // 但 next_run 仍应把连续的 5 个 piece 合成一次请求
        assert_eq!(back.next_run(), Some((0, 5)));
    }

    #[test]
    fn bitfield_partial_holes() {
        // 4 个 piece，第 0 和 2 完成
        let pl = 1024u64;
        let total = 4 * pl;
        let bits = vec![0b0000_0101u8]; // bit0 和 bit2
        let t = SegmentTable::from_bitfield(total, pl, &bits);

        // 应恢复成 Done(0..pl-1), Missing(pl..2pl-1), Done(2pl..3pl-1), Missing(3pl..4pl-1)
        assert_eq!(t.segments.len(), 4);
        assert!(t.segments[0].state.is_done());
        assert_eq!(t.segments[0].start, 0);
        assert_eq!(t.segments[0].end, pl - 1);
        assert!(matches!(t.segments[1].state, SegState::Missing));
        assert_eq!(t.segments[1].start, pl);
        assert!(t.segments[2].state.is_done());
        assert!(matches!(t.segments[3].state, SegState::Missing));
        assert_eq!(t.completed(), 2 * pl);
    }

    #[test]
    fn split_table_covers_total() {
        let total = 64 * 1024 * 1024;
        let t = SegmentTable::split(total, 6);
        assert_eq!(t.segments.len(), 6);
        assert_eq!(t.segments[0].start, 0);
        assert_eq!(t.segments.last().unwrap().end, total - 1);
        let sum: u64 = t.segments.iter().map(|s| s.len()).sum();
        assert_eq!(sum, total, "切分丢字节");
    }

    #[test]
    fn next_missing_skips_done() {
        let pl = 1024u64;
        let total = 4 * pl;
        let bits = vec![0b0000_0001u8]; // 只有 piece0 完成
        let t = SegmentTable::from_bitfield(total, pl, &bits);
        let idx = t.next_missing().unwrap();
        assert_eq!(t.segments[idx].start, pl);
    }

    #[test]
    fn next_run_limited_respects_max() {
        let pl = 1024u64;
        let total = 10 * pl;
        let t = SegmentTable::from_bitfield(total, pl, &[]);
        // 全部 Missing，10 个 piece；限 3 个
        assert_eq!(t.next_run_limited(3), Some((0, 3)));
        assert_eq!(t.next_run_limited(100), Some((0, 10)));
    }

    #[test]
    fn concurrent_runs_are_disjoint() {
        let pl = 1024u64;
        let total = 10 * pl;
        let mut t = SegmentTable::from_bitfield(total, pl, &[]);
        // worker0 领 3 个
        let (s0, c0) = t.next_run_limited(3).unwrap();
        assert!(t.assign_run(s0, c0, 0, 100));
        // worker1 应从第 3 个开始，不与 worker0 重叠
        let (s1, c1) = t.next_run_limited(3).unwrap();
        assert_eq!(s1, 3);
        assert!(t.assign_run(s1, c1, 1, 101));
        // worker2
        let (s2, c2) = t.next_run_limited(3).unwrap();
        assert_eq!(s2, 6);
        assert!(t.assign_run(s2, c2, 2, 102));
        // 只剩最后 1 个
        assert_eq!(t.next_run_limited(3), Some((9, 1)));
    }

    #[test]
    fn reclaim_run_preserves_done_pieces() {
        let pl = 1024u64;
        let total = 4 * pl;
        let mut t = SegmentTable::from_bitfield(total, pl, &[]);
        // 让 worker0 领 4 个，报告写了 2.5 个 piece（前 2 个应转 Done）
        t.assign_run(0, 4, 0, 7);
        t.report_run_progress(0, 4, 7, 2 * pl + 100);
        assert!(t.segments[0].state.is_done());
        assert!(t.segments[1].state.is_done());
        assert!(matches!(t.segments[2].state, SegState::Active { .. }));

        // 抢占整段
        assert!(t.begin_reclaim_run(0, 4, 0, 7));
        assert!(t.segments[0].state.is_done()); // Done 不动
        assert!(matches!(t.segments[2].state, SegState::Reclaiming { .. }));
        assert_eq!(t.preempts_at(2), 1);

        // worker 确认 Stopped
        assert!(t.finish_reclaim_run(0, 4, 7));
        assert!(t.segments[0].state.is_done()); // Done 保留
        assert!(matches!(t.segments[1].state, SegState::Done));
        assert!(matches!(t.segments[2].state, SegState::Missing));
        assert!(matches!(t.segments[3].state, SegState::Missing));
        // 已完成 2 片
        assert_eq!(t.completed(), 2 * pl);
    }

    #[test]
    fn reclaim_accepts_done_racing_in() {
        // worker 在收到 Stop 的同时刚好写完 → 发 Done 而不是 Stopped。
        // 此时 piece 已是 Reclaiming，report_run_progress 必须仍能把它置 Done。
        let pl = 1024u64;
        let total = 2 * pl;
        let mut t = SegmentTable::from_bitfield(total, pl, &[]);
        t.assign_run(0, 2, 0, 9);
        assert!(t.begin_reclaim_run(0, 2, 0, 9));
        // 全部写完
        let newly = t.report_run_progress(0, 2, 9, 2 * pl);
        assert_eq!(newly, 2 * pl);
        assert!(t.segments[0].state.is_done());
        assert!(t.segments[1].state.is_done());
        assert!(t.is_done());
    }

    #[test]
    fn bump_attempts_counts_per_piece() {
        let pl = 1024u64;
        let total = 2 * pl;
        let mut t = SegmentTable::from_bitfield(total, pl, &[]);
        t.assign_run(0, 2, 0, 5);
        assert_eq!(t.bump_attempts_run(0, 2, 5), 1);
        t.finish_run(0, 2, 5); // 退回 Missing
        t.assign_run(0, 2, 0, 6);
        assert_eq!(t.bump_attempts_run(0, 2, 6), 2);
    }
}
