//! 紧凑胶囊尺寸、悬停波浪曲线、物理弹簧插值、命中检测（纯数学，可单元测试）。

/// 单个图标槽位布局（窗口内坐标）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Slot {
    pub center_x: f32,
    pub center_y: f32,
    pub size: f32,
    pub scale: f32,
}

#[derive(Clone, Debug, Default)]
pub struct DockLayout {
    /// 胶囊尺寸
    pub capsule_w: f32,
    pub capsule_h: f32,
    pub slots: Vec<Slot>,
    /// 竖向分隔线中心 x（窗口坐标）
    pub separators: Vec<f32>,
    /// 图标可能向上突破胶囊的最大高度（用于窗口预留）
    pub grow_top: f32,
}

/// 悬停影响因子：鼠标在图标中心为 1，向两侧按余弦衰减，超出 range 归零。
pub fn influence(distance: f32, pitch: f32, range: f32) -> f32 {
    if range <= 0.0 || pitch <= 0.0 {
        return 0.0;
    }
    let d = distance / (pitch * range);
    if d >= 1.0 {
        0.0
    } else {
        (d * std::f32::consts::PI).cos() * 0.5 + 0.5
    }
}

/// 半隐式欧拉弹簧步进（k 刚度、ζ 阻尼比）。返回 (位置, 速度)。
pub fn spring_step(cur: f32, vel: &mut f32, target: f32, dt: f32, k: f32, zeta: f32) -> f32 {
    let dt = dt.clamp(0.0, 0.05);
    let c = 2.0 * k.sqrt() * zeta;
    let a = -k * (cur - target) - c * *vel;
    *vel += a * dt;
    cur + *vel * dt
}

/// 指数趋近（用于显隐滑动与颜色过渡），reduce_motion 时直接到位。
pub fn ease_to(cur: f32, target: f32, dt_ms: f32, duration_ms: f32, reduce: bool) -> f32 {
    if reduce || duration_ms <= 0.0 || (cur - target).abs() < 0.001 {
        return target;
    }
    // 时间常数：约 duration 内完成 95%
    let tau = (duration_ms / 3.0).max(1.0);
    let t = 1.0 - (-dt_ms / tau).exp();
    cur + (target - cur) * t
}

/// 计算胶囊布局。
///
/// * `scales`：每个条目当前缩放（1.0 为基础）
/// * `separator_after`：需要在其后插入分隔线的条目索引（已排序）
pub fn compute(
    count: usize,
    scales: &[f32],
    separator_after: &[usize],
    icon: f32,
    gap: f32,
    pad_x: f32,
    pad_y: f32,
    max_scale: f32,
) -> DockLayout {
    let mut layout = DockLayout::default();
    if count == 0 || scales.len() != count {
        return layout;
    }
    // 分隔线占位 = gap + 1px 线 + gap
    let sep_w = gap * 2.0 + 1.0;
    let has_sep_after = |i: usize| separator_after.contains(&i) && i + 1 < count;

    let mut x = pad_x;
    let mut prev_half = 0.0f32;
    let mut centers: Vec<f32> = Vec::with_capacity(count);
    for i in 0..count {
        let w = icon * scales[i];
        if i == 0 {
            x += w * 0.5;
        } else {
            x += prev_half + gap + w * 0.5;
            if has_sep_after(i - 1) {
                x += sep_w - gap;
            }
        }
        centers.push(x);
        prev_half = w * 0.5;
    }
    layout.capsule_w = (x + prev_half + pad_x).max(icon + pad_x * 2.0);
    layout.capsule_h = icon + pad_y * 2.0;
    let mut grow = 0.0f32;
    let base_cy = layout.capsule_h - pad_y - icon * 0.5;
    for i in 0..count {
        let s = scales[i];
        let size = icon * s;
        let cy = (layout.capsule_h - pad_y) - size * 0.5;
        grow = grow.max(base_cy - cy + (size - icon) * 0.5);
        layout.slots.push(Slot {
            center_x: centers[i],
            center_y: cy,
            size,
            scale: s,
        });
        if has_sep_after(i) {
            let right = centers[i] + size * 0.5 + gap;
            let next_left = centers[i + 1] - icon * scales[i + 1] * 0.5 - gap;
            layout.separators.push((right + next_left) * 0.5);
        }
    }
    layout.grow_top = grow.max(icon * (max_scale - 1.0));
    layout
}

/// 命中检测：返回槽位索引。
pub fn hit_slot(layout: &DockLayout, x: f32, y: f32, extra: f32) -> Option<usize> {
    let mut best: Option<(f32, usize)> = None;
    for (i, s) in layout.slots.iter().enumerate() {
        let half = s.size * 0.5 + extra;
        if x >= s.center_x - half && x <= s.center_x + half && y >= s.center_y - half && y <= s.center_y + half {
            let d = (x - s.center_x).abs();
            if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                best = Some((d, i));
            }
        }
    }
    best.map(|(_, i)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn influence_decays_to_zero() {
        assert!((influence(0.0, 50.0, 2.0) - 1.0).abs() < 1e-4);
        assert_eq!(influence(150.0, 50.0, 2.0), 0.0);
        let mid = influence(50.0, 50.0, 2.0);
        // 升余弦：半程处恰为 0.5
        assert!((mid - 0.5).abs() < 1e-4);
    }

    #[test]
    fn spring_converges() {
        let mut v = 0.0;
        let mut cur = 1.0;
        for _ in 0..120 {
            cur = spring_step(cur, &mut v, 1.5, 1.0 / 60.0, 180.0, 0.75);
        }
        assert!((cur - 1.5).abs() < 0.01);
    }

    #[test]
    fn layout_width_grows_with_magnification() {
        let base = compute(5, &[1.0; 5], &[], 48.0, 8.0, 10.0, 8.0, 1.45);
        let mag = compute(5, &[1.0, 1.45, 1.45, 1.0, 1.0], &[], 48.0, 8.0, 10.0, 8.0, 1.45);
        assert!(mag.capsule_w > base.capsule_w);
        assert_eq!(base.slots.len(), 5);
        assert_eq!(hit_slot(&base, base.slots[2].center_x, base.slots[2].center_y, 0.0), Some(2));
        assert_eq!(hit_slot(&base, -50.0, -50.0, 0.0), None);
        // 放大图标中心上移（向上突破胶囊）
        assert!(mag.slots[1].center_y < base.slots[1].center_y);
    }

    #[test]
    fn separators_are_between_entries() {
        let l = compute(4, &[1.0; 4], &[1], 48.0, 8.0, 10.0, 8.0, 1.45);
        assert_eq!(l.separators.len(), 1);
        assert!(l.separators[0] > l.slots[1].center_x && l.separators[0] < l.slots[2].center_x);
    }
}
