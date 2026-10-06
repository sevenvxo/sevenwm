//! wobbly windows where a floating window u drag bends like jelly and springs back once it stops

use std::cell::RefCell;
use std::time::Instant;

use smithay::desktop::Window;

/// the spring grid is n by n points spread over the window
pub const N: usize = 4;
/// how hard each point pulls back to where it belongs
const STIFFNESS: f64 = 220.0;
/// how fast the jiggle dies down so lower wobbles longer
const DAMPING: f64 = 13.0;
/// physics steps this long so a slow frame cant blow it up
const STEP: f64 = 1.0 / 240.0;

/// one windows jelly w each points offset from where it should be in canvas px
#[derive(Default)]
pub struct Wobble {
    offset: [[[f64; 2]; N]; N],
    velocity: [[[f64; 2]; N]; N],
    /// where u grabbed it from 0 to 1 across the window
    grab: [f64; 2],
    last_pos: Option<[f64; 2]>,
    last_tick: Option<Instant>,
    dragging: bool,
}

#[derive(Default)]
struct WobbleData(RefCell<Option<Wobble>>);

/// u grabbed the window at uv so the far side should lag behind
pub fn grab(window: &Window, uv: [f64; 2]) {
    let data = window.user_data().get_or_insert(WobbleData::default);
    let mut slot = data.0.borrow_mut();
    let w = slot.get_or_insert_with(Wobble::default);
    w.grab = [uv[0].clamp(0.0, 1.0), uv[1].clamp(0.0, 1.0)];
    w.dragging = true;
    w.last_pos = None;
}

/// u let go so it settles on its own
pub fn release(window: &Window) {
    if let Some(data) = window.user_data().get::<WobbleData>()
        && let Some(w) = data.0.borrow_mut().as_mut()
    {
        w.dragging = false;
    }
}

/// stop the jiggle right away like when the window became a tile or wobbly got turned off
pub fn stop(window: &Window) {
    if let Some(data) = window.user_data().get::<WobbleData>() {
        data.0.borrow_mut().take();
    }
}

/// whether the window is bending rn so its border shadow and title bar wait
pub fn active(window: &Window) -> bool {
    window
        .user_data()
        .get::<WobbleData>()
        .is_some_and(|d| d.0.borrow().is_some())
}

/// step the springs to now w the window at pos and return the offsets or none once its still
pub fn step(window: &Window, pos: [f64; 2], size: [f64; 2]) -> Option<[[[f64; 2]; N]; N]> {
    let data = window.user_data().get::<WobbleData>()?;
    let mut slot = data.0.borrow_mut();
    let w = slot.as_mut()?;
    let now = Instant::now();
    // the window moved so points far from the grab get left behind
    if let Some(last) = w.last_pos {
        let d = [pos[0] - last[0], pos[1] - last[1]];
        if d != [0.0, 0.0] {
            for (i, row) in w.offset.iter_mut().enumerate() {
                for (j, off) in row.iter_mut().enumerate() {
                    let (u, v) = (j as f64 / (N - 1) as f64, i as f64 / (N - 1) as f64);
                    let lag = ((u - w.grab[0]).hypot(v - w.grab[1]) * 1.3).min(1.0);
                    off[0] -= d[0] * lag;
                    off[1] -= d[1] * lag;
                }
            }
        }
    }
    w.last_pos = Some(pos);
    let dt = w.last_tick.map_or(0.0, |t| now.duration_since(t).as_secs_f64().min(0.05));
    w.last_tick = Some(now);
    let mut left = dt;
    while left > 0.0 {
        let h = left.min(STEP);
        left -= h;
        for (row_o, row_v) in w.offset.iter_mut().zip(w.velocity.iter_mut()) {
            for (off, vel) in row_o.iter_mut().zip(row_v.iter_mut()) {
                for k in 0..2 {
                    let acc = -STIFFNESS * off[k] - DAMPING * vel[k];
                    vel[k] += acc * h;
                    off[k] += vel[k] * h;
                }
            }
        }
    }
    // a huge fling cant tear it apart
    let limit = [size[0] * 0.35, size[1] * 0.35];
    let mut still = !w.dragging;
    for (row_o, row_v) in w.offset.iter_mut().zip(w.velocity.iter()) {
        for (off, vel) in row_o.iter_mut().zip(row_v.iter()) {
            for k in 0..2 {
                off[k] = off[k].clamp(-limit[k], limit[k]);
                if off[k].abs() > 0.15 || vel[k].abs() > 0.5 {
                    still = false;
                }
            }
        }
    }
    if still {
        *slot = None;
        return None;
    }
    Some(w.offset)
}

/// the offsets packed into two column major 4x4 matrices the shader reads as mat4 grid[2]
pub fn pack(offset: &[[[f64; 2]; N]; N], scale: f64) -> Vec<[f32; 16]> {
    let mut m = vec![[0.0f32; 16]; 2];
    for i in 0..N {
        for j in 0..N {
            let k = i * N + j;
            let (which, col, pair) = (k / 8, (k % 8) / 2, (k % 2) * 2);
            m[which][col * 4 + pair] = (offset[i][j][0] * scale) as f32;
            m[which][col * 4 + pair + 1] = (offset[i][j][1] * scale) as f32;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_puts_each_point_where_the_shader_looks() {
        let mut off = [[[0.0; 2]; N]; N];
        off[2][1] = [3.0, -4.0];
        // k is 9 so the second matrix column 0 zw bc odd points sit in the back half
        let m = pack(&off, 2.0);
        assert_eq!(m[1][2], 6.0);
        assert_eq!(m[1][3], -8.0);
    }
}
