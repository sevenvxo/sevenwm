//! pure geometry for tiling snapping and free space w no compositor state so its all tested

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::config::Direction;

pub type Rect = Rectangle<i32, Logical>;

/// split area dwindle style where each window takes its ratio of whats left and the last gets the rest
pub fn dwindle(area: Rect, ratios: &[f64], gap: i32) -> Vec<Rect> {
    let mut remaining = area;
    let mut tiles = Vec::with_capacity(ratios.len());
    for (i, &ratio) in ratios.iter().enumerate() {
        if i + 1 == ratios.len() || !can_cut(remaining, gap) {
            tiles.push(remaining);
            continue;
        }
        let (_, tile, rest) = cut(remaining, ratio, gap);
        tiles.push(tile);
        remaining = rest;
    }
    tiles
}

/// whats left of area for this tile and the ones after it
pub fn dwindle_remaining(area: Rect, ratios: &[f64], gap: i32, tile: usize) -> Rect {
    let mut remaining = area;
    for &ratio in ratios.iter().take(tile.min(ratios.len().saturating_sub(1))) {
        if !can_cut(remaining, gap) {
            break;
        }
        remaining = cut(remaining, ratio, gap).2;
    }
    remaining
}

/// the ratio that makes a cut give about size so a window keeps its width as it tiles
pub fn ratio_for(remaining: Rect, size: Size<i32, Logical>, gap: i32) -> f64 {
    let (extent, span) = if remaining.size.w >= remaining.size.h {
        (size.w, remaining.size.w)
    } else {
        (size.h, remaining.size.h)
    };
    (extent as f64 / (span - gap).max(1) as f64).clamp(0.1, 0.9)
}

/// which split makes this tiles edge and how many pixels it divides
pub fn dwindle_edge(
    area: Rect,
    ratios: &[f64],
    gap: i32,
    tile: usize,
    dir: Direction,
) -> Option<(usize, f64)> {
    let mut remaining = area;
    // each cut puts the earlier tile left or above so edges come from earlier cuts
    let mut before = None;
    for (j, &ratio) in ratios
        .iter()
        .enumerate()
        .take(ratios.len().saturating_sub(1))
    {
        // tiles stacked in a leftover too small to cut have no inner edges
        if !can_cut(remaining, gap) {
            return before.filter(|_| matches!(dir, Direction::Left | Direction::Up));
        }
        let (vertical, _, rest) = cut(remaining, ratio, gap);
        let span = if vertical {
            remaining.size.w
        } else {
            remaining.size.h
        } - gap;
        let span = span.max(1) as f64;
        if j == tile {
            return match (vertical, dir) {
                (true, Direction::Right) | (false, Direction::Down) => Some((j, span)),
                (_, Direction::Left | Direction::Up) => before,
                _ => None,
            };
        }
        match (vertical, dir) {
            (true, Direction::Left) | (false, Direction::Up) => before = Some((j, -span)),
            _ => {}
        }
        remaining = rest;
    }
    // the last tiles right and bottom edges are the areas
    match dir {
        Direction::Left | Direction::Up if tile + 1 == ratios.len() => before,
        _ => None,
    }
}

/// the smallest a tile side gets from dwindle cuts
pub const MIN_TILE: i32 = 60;

/// whether area is big enough to cut into two uhhh tiles
fn can_cut(area: Rect, gap: i32) -> bool {
    area.size.w.max(area.size.h) >= 2 * MIN_TILE + gap
}

/// one dwindle cut that says if its side by side plus the tile and whats left
fn cut(area: Rect, ratio: f64, gap: i32) -> (bool, Rect, Rect) {
    let ratio = ratio.clamp(0.1, 0.9);
    let Rectangle { loc, size } = area;
    // neither side smaller than MIN_TILE when theres room
    let limit = |len: i32| {
        let lo = MIN_TILE.min((len - gap) / 2).max(1);
        move |first: i32| first.clamp(lo, (len - gap - lo).max(lo))
    };
    if size.w >= size.h {
        let first = limit(size.w)((((size.w - gap) as f64) * ratio).round() as i32);
        (
            true,
            Rectangle::new(loc, Size::from((first, size.h))),
            Rectangle::new(
                Point::from((loc.x + first + gap, loc.y)),
                Size::from((size.w - first - gap, size.h)),
            ),
        )
    } else {
        let first = limit(size.h)((((size.h - gap) as f64) * ratio).round() as i32);
        (
            false,
            Rectangle::new(loc, Size::from((size.w, first))),
            Rectangle::new(
                Point::from((loc.x, loc.y + first + gap)),
                Size::from((size.w, size.h - first - gap)),
            ),
        )
    }
}

/// shrink rect by by on every side
pub fn inset(rect: Rect, by: i32) -> Rect {
    Rectangle::new(
        rect.loc + Point::from((by, by)),
        Size::from(((rect.size.w - 2 * by).max(1), (rect.size.h - 2 * by).max(1))),
    )
}

/// grow rect by by on every side
pub fn outset(rect: Rect, by: i32) -> Rect {
    Rectangle::new(
        rect.loc - Point::from((by, by)),
        Size::from((rect.size.w + 2 * by, rect.size.h + 2 * by)),
    )
}

fn centre(rect: Rect) -> (f64, f64) {
    (
        rect.loc.x as f64 + rect.size.w as f64 / 2.0,
        rect.loc.y as f64 + rect.size.h as f64 / 2.0,
    )
}

/// where rect should move so its edges snap to targets on each axis within threshold
pub fn snap(rect: Rect, targets: &[Rect], gap: i32, threshold: i32) -> Point<i32, Logical> {
    let (x, y) = (rect.loc.x, rect.loc.y);
    let (w, h) = (rect.size.w, rect.size.h);
    let mut best_x: Option<(i32, i32)> = None;
    let mut best_y: Option<(i32, i32)> = None;
    let consider = |best: &mut Option<(i32, i32)>, current: i32, candidate: i32| {
        let dist = (candidate - current).abs();
        if dist <= threshold && best.is_none_or(|(_, d)| dist < d) {
            *best = Some((candidate, dist));
        }
    };
    let reach = gap + threshold;
    for t in targets {
        let (tx, ty, tw, th) = (t.loc.x, t.loc.y, t.size.w, t.size.h);
        let overlap_x = x < tx + tw && tx < x + w;
        let overlap_y = y < ty + th && ty < y + h;
        let near_x = x < tx + tw + reach && tx < x + w + reach;
        let near_y = y < ty + th + reach && ty < y + h + reach;
        if overlap_y {
            consider(&mut best_x, x, tx - gap - w);
            consider(&mut best_x, x, tx + tw + gap);
        }
        if near_y {
            consider(&mut best_x, x, tx);
            consider(&mut best_x, x, tx + tw - w);
        }
        if overlap_x {
            consider(&mut best_y, y, ty - gap - h);
            consider(&mut best_y, y, ty + th + gap);
        }
        if near_x {
            consider(&mut best_y, y, ty);
            consider(&mut best_y, y, ty + th - h);
        }
    }
    Point::from((best_x.map_or(x, |b| b.0), best_y.map_or(y, |b| b.0)))
}

/// the spot for a rect that hits nothing and stays in bounds as close to target as it can
pub fn free_spot(
    size: Size<i32, Logical>,
    target: Point<f64, Logical>,
    obstacles: &[Rect],
    gap: i32,
    bounds: Option<Rect>,
) -> Option<Point<i32, Logical>> {
    let (w, h) = (size.w, size.h);
    let at_target = Point::from((
        (target.x - w as f64 / 2.0).round() as i32,
        (target.y - h as f64 / 2.0).round() as i32,
    ));
    let mut candidates = vec![at_target];
    for o in obstacles {
        let (ox, oy, ow, oh) = (o.loc.x, o.loc.y, o.size.w, o.size.h);
        let xs_beside = [ox - gap - w, ox + ow + gap];
        let ys_beside = [oy - gap - h, oy + oh + gap];
        // along each side line up w either end or the target
        let xs_along = [ox, ox + ow - w, at_target.x];
        let ys_along = [oy, oy + oh - h, at_target.y];
        for x in xs_beside {
            candidates.extend(ys_along.iter().map(|&y| Point::from((x, y))));
        }
        for y in ys_beside {
            candidates.extend(xs_along.iter().map(|&x| Point::from((x, y))));
        }
    }
    let free = |p: &Point<i32, Logical>| {
        let rect = Rectangle::new(*p, size);
        bounds.is_none_or(|b| b.contains_rect(rect))
            && !obstacles.iter().any(|o| outset(*o, gap - 1).overlaps(rect))
    };
    let distance = |p: &Point<i32, Logical>| {
        let (cx, cy) = centre(Rectangle::new(*p, size));
        (cx - target.x).powi(2) + (cy - target.y).powi(2)
    };
    candidates
        .into_iter()
        .filter(free)
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))
}

/// whether other sits snapped against rects side w the gap between them give or take a pixel or two
pub fn beside(rect: Rect, other: Rect, side: Direction, gap: i32) -> bool {
    let (a, o) = (rect, other);
    let overlap_x = a.loc.x < o.loc.x + o.size.w && o.loc.x < a.loc.x + a.size.w;
    let overlap_y = a.loc.y < o.loc.y + o.size.h && o.loc.y < a.loc.y + a.size.h;
    let near = |x: i32, y: i32| (x - y).abs() <= 2;
    match side {
        Direction::Left => overlap_y && near(o.loc.x + o.size.w + gap, a.loc.x),
        Direction::Right => overlap_y && near(a.loc.x + a.size.w + gap, o.loc.x),
        Direction::Up => overlap_x && near(o.loc.y + o.size.h + gap, a.loc.y),
        Direction::Down => overlap_x && near(a.loc.y + a.size.h + gap, o.loc.y),
    }
}

/// move rect the least it needs to fit inside bounds
pub fn clamp_into(rect: Rect, bounds: Rect) -> Point<i32, Logical> {
    let clamp = |pos: i32, len: i32, lo: i32, span: i32| {
        if len >= span {
            lo
        } else {
            pos.clamp(lo, lo + span - len)
        }
    };
    Point::from((
        clamp(rect.loc.x, rect.size.w, bounds.loc.x, bounds.size.w),
        clamp(rect.loc.y, rect.size.h, bounds.loc.y, bounds.size.h),
    ))
}

/// which rect is nearest from origin going dir w near and straight first
pub fn nearest(origin: Point<f64, Logical>, dir: Direction, rects: &[Rect]) -> Option<usize> {
    let (ux, uy) = dir.delta();
    let (ux, uy) = (ux as f64, uy as f64);
    rects
        .iter()
        .enumerate()
        .filter_map(|(i, rect)| {
            let r = rect.to_f64();
            let closest = Point::<f64, Logical>::from((
                origin.x.clamp(r.loc.x, r.loc.x + r.size.w),
                origin.y.clamp(r.loc.y, r.loc.y + r.size.h),
            ));
            // inside it so its center gives the direction
            let to = if closest == origin {
                Point::from((r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0))
            } else {
                closest
            };
            let (dx, dy) = (to.x - origin.x, to.y - origin.y);
            let ahead = dx * ux + dy * uy;
            // distance over cos angle so near and straight ahead wins maybe
            (ahead > 0.0).then(|| ((dx * dx + dy * dy) / ahead, i))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, i)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beside_needs_the_gap_and_some_overlap() {
        let a = Rect::new(Point::from((100, 0)), Size::from((100, 100)));
        let left = Rect::new(Point::from((0, 50)), Size::from((90, 100)));
        assert!(beside(a, left, Direction::Left, 10));
        assert!(!beside(a, left, Direction::Right, 10));
        let far_down = Rect::new(Point::from((0, 200)), Size::from((90, 100)));
        assert!(!beside(a, far_down, Direction::Left, 10));
    }

    #[test]
    fn a_ratio_for_a_size_gives_that_size() {
        let area = Rect::new(Point::from((0, 0)), Size::from((1000, 600)));
        let ratio = ratio_for(area, Size::from((300, 500)), 10);
        let tiles = dwindle(area, &[ratio, 0.5], 10);
        assert_eq!(tiles[0].size.w, 300);
        // after the first cut the second tiles cut divides whats left
        let rest = dwindle_remaining(area, &[ratio, 0.5, 0.5], 10, 1);
        assert_eq!(rest, tiles[1]);
    }

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn dwindle_one_window_fills_the_area() {
        assert_eq!(
            dwindle(r(0, 0, 1000, 600), &[0.5], 10),
            vec![r(0, 0, 1000, 600)]
        );
    }

    #[test]
    fn dwindle_splits_the_longer_side_and_keeps_gaps() {
        let tiles = dwindle(r(0, 0, 1010, 600), &[0.5, 0.5, 0.5], 10);
        assert_eq!(tiles[0], r(0, 0, 500, 600), "wide: split left/right");
        assert_eq!(
            tiles[1],
            r(510, 0, 500, 295),
            "the rest is tall: split top/bottom"
        );
        assert_eq!(tiles[2], r(510, 305, 500, 295));
    }

    #[test]
    fn dwindle_edge_finds_the_split_on_each_side() {
        use Direction::*;
        // 1010x600 w tile 0 left and tiles 1 and 2 on the right
        let area = r(0, 0, 1010, 600);
        let ratios = [0.5, 0.5, 0.5];
        let edge = |tile, dir| dwindle_edge(area, &ratios, 10, tile, dir);
        assert_eq!(edge(0, Right), Some((0, 1000.0)));
        assert_eq!(edge(0, Left), None);
        assert_eq!(edge(0, Up), None);
        assert_eq!(edge(0, Down), None);
        assert_eq!(edge(1, Left), Some((0, -1000.0)));
        assert_eq!(edge(1, Down), Some((1, 590.0)));
        assert_eq!(edge(1, Up), None);
        assert_eq!(edge(1, Right), None);
        assert_eq!(edge(2, Left), Some((0, -1000.0)));
        assert_eq!(edge(2, Up), Some((1, -590.0)));
        assert_eq!(edge(2, Down), None);
        assert_eq!(edge(2, Right), None);
        // a lone tile fills the area so nothing to move
        assert_eq!(dwindle_edge(area, &[0.5], 10, 0, Left), None);
    }

    #[test]
    fn dwindle_tiles_never_overlap() {
        for n in 1..8 {
            let tiles = dwindle(r(0, 0, 1920, 1080), &vec![0.5; n], 8);
            assert_eq!(tiles.len(), n);
            for (i, a) in tiles.iter().enumerate() {
                for b in &tiles[i + 1..] {
                    assert!(!a.overlaps(*b), "{n} tiles: {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn dwindle_ratio_gives_the_main_window_more() {
        let tiles = dwindle(r(0, 0, 1000, 500), &[0.7, 0.5], 0);
        assert_eq!(tiles[0].size.w, 700);
    }

    #[test]
    fn snap_sits_beside_a_neighbour_with_the_gap() {
        let other = r(0, 0, 100, 100);
        let moved = r(118, 10, 50, 50);
        assert_eq!(snap(moved, &[other], 10, 16), Point::from((110, 0)));
    }

    #[test]
    fn snap_ignores_targets_out_of_reach() {
        let other = r(0, 0, 100, 100);
        let moved = r(300, 300, 50, 50);
        assert_eq!(snap(moved, &[other], 10, 16), moved.loc);
    }

    #[test]
    fn free_spot_is_the_target_when_nothing_is_in_the_way() {
        let spot = free_spot(
            Size::from((100, 100)),
            Point::from((500.0, 500.0)),
            &[],
            10,
            None,
        );
        assert_eq!(spot, Some(Point::from((450, 450))));
    }

    #[test]
    fn free_spot_moves_beside_an_obstacle_at_the_target() {
        let obstacle = r(400, 400, 200, 200);
        let spot = free_spot(
            Size::from((100, 100)),
            Point::from((500.0, 500.0)),
            &[obstacle],
            10,
            None,
        )
        .unwrap();
        let placed = Rectangle::new(spot, Size::from((100, 100)));
        assert!(!outset(obstacle, 9).overlaps(placed));
        // right beside it not somewhere far
        let (cx, cy) = centre(placed);
        assert!(((cx - 500.0).powi(2) + (cy - 500.0).powi(2)).sqrt() <= 160.0);
    }

    #[test]
    fn free_spot_respects_bounds() {
        let bounds = r(0, 0, 300, 300);
        let spot = free_spot(
            Size::from((100, 100)),
            Point::from((290.0, 290.0)),
            &[r(0, 0, 100, 100)],
            10,
            Some(bounds),
        )
        .unwrap();
        assert!(bounds.contains_rect(Rectangle::new(spot, Size::from((100, 100)))));
    }

    #[test]
    fn clamp_into_moves_the_least() {
        assert_eq!(
            clamp_into(r(-20, 50, 100, 100), r(0, 0, 500, 500)),
            Point::from((0, 50))
        );
        assert_eq!(
            clamp_into(r(450, 450, 100, 100), r(0, 0, 500, 500)),
            Point::from((400, 400))
        );
    }

    #[test]
    fn nearest_goes_the_right_way() {
        let origin = Point::from((500.0, 500.0));
        let rects = [r(0, 400, 100, 200), r(900, 400, 100, 200), r(1500, 400, 100, 200)];
        assert_eq!(nearest(origin, Direction::Right, &rects), Some(1));
        assert_eq!(nearest(origin, Direction::Left, &rects), Some(0));
        assert_eq!(nearest(origin, Direction::Up, &rects), None);
    }

    #[test]
    fn nearest_measures_to_the_closest_edge() {
        // a big window right next to u beats a small one further straight ahead
        let origin = Point::from((500.0, 500.0));
        let rects = [r(600, 520, 1000, 2000), r(1200, 450, 100, 100)];
        assert_eq!(nearest(origin, Direction::Right, &rects), Some(0));
    }

    #[test]
    fn nearest_prefers_straight_ahead() {
        let origin = Point::from((0.0, 0.0));
        // same distance but one straight right and one steeply down right
        let rects = [r(1000, -10, 20, 20), r(300, 950, 20, 20)];
        assert_eq!(nearest(origin, Direction::Right, &rects), Some(0));
    }

    #[test]
    fn many_tiles_never_collapse() {
        let area = Rectangle::new(Point::from((0, 0)), Size::from((1920, 1080)));
        let tiles = dwindle(area, &[0.5; 30], 8);
        assert_eq!(tiles.len(), 30);
        for tile in tiles {
            assert!(tile.size.w >= 1 && tile.size.h >= 1, "{tile:?}");
        }
    }
}
