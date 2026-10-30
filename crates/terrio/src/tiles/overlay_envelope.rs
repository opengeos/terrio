//! Floating-precision intersection envelopes from JTS 1.20.0
//! `operation/overlayng/{OverlayUtil,RobustClipEnvelopeComputer}`.
//! Copyright (c) 2019, 2020 Martin Davis. EDL-1.0; see NOTICE.
//!
//! This computes an envelope, not an overlay or a MakeValid operation.

use super::clip::{Pt, Rect};

/// JTS OverlayUtil.safeEnv for a floating precision model.
fn buffered(env: Rect) -> Rect {
    let width = env.2 - env.0;
    let height = env.3 - env.1;
    let minimum = width.min(height);
    let distance = 0.1 * if minimum > 0.0 { minimum } else { width.max(height) };
    (env.0 - distance, env.1 - distance, env.2 + distance, env.3 + distance)
}

fn segment_meets(env: Rect, a: Pt, b: Pt) -> bool {
    a.0.max(b.0) >= env.0 && a.0.min(b.0) <= env.2 && a.1.max(b.1) >= env.1 && a.1.min(b.1) <= env.3
}

/// Intersection specialization of JTS OverlayUtil.clippingEnvelope. All polygon
/// rings and the rectangle are supplied to RobustClipEnvelopeComputer: an edge
/// whose envelope meets the target contributes both endpoints. The target stays
/// fixed while its enclosing clipping envelope grows.
pub(super) fn intersection(polygons: &[Vec<Vec<Pt>>], bounds: Rect, rectangle: Rect) -> Option<Rect> {
    let a = buffered(bounds);
    let b = buffered(rectangle);
    let target = (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3));
    if target.0 > target.2 || target.1 > target.3 {
        return None;
    }
    let mut envelope = target;
    let mut add_ring = |ring: &[Pt]| {
        for segment in ring.windows(2) {
            let (a, b) = (segment[0], segment[1]);
            if segment_meets(target, a, b) {
                for point in [a, b] {
                    envelope.0 = envelope.0.min(point.0);
                    envelope.1 = envelope.1.min(point.1);
                    envelope.2 = envelope.2.max(point.0);
                    envelope.3 = envelope.3.max(point.1);
                }
            }
        }
    };
    for polygon in polygons {
        for ring in polygon {
            add_ring(ring);
        }
    }
    add_ring(&[
        (rectangle.0, rectangle.1),
        (rectangle.2, rectangle.1),
        (rectangle.2, rectangle.3),
        (rectangle.0, rectangle.3),
        (rectangle.0, rectangle.1),
    ]);
    Some(buffered(envelope))
}
