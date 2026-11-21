//! Camera math: turns the cap's deflection into camera motion.
//!
//! The cap is a velocity controller: its deflection sets a speed, and each frame moves the
//! camera by speed × frame time, so motion is the same whatever the frame rate.
//!
//! Conventions (as Onshape's navigation library uses them):
//! - `view.affine` is the camera-to-world transform, a 4×4 matrix stored column-major:
//!   columns are camera right, camera up, camera back (−forward), and the eye position.
//! - Camera frame: +x right on screen, +y up on screen, +z out of the screen toward the user.
//! - `view.extents` (orthographic) is `[left, bottom, -far, right, top, -near]`.
//! - `view.frustum` (perspective) is `[left, right, bottom, top, near, far]`.

use glam::{DMat3, DMat4, DQuat, DVec3, DVec4};

/// Full-scale deflection reported by 3Dconnexion devices.
pub const AXIS_FULL_SCALE: f64 = 350.0;

/// Axis names in the camera frame, in the order used by [`Settings::invert`].
pub const AXIS_NAMES: [&str; 6] = ["pan-x", "pan-y", "zoom", "pitch", "yaw", "roll"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The model follows the cap (3DxWare's default "object mode").
    Object,
    /// The camera follows the cap, orbiting the pivot ("target camera mode").
    Camera,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub mode: Mode,
    /// View heights per second at full deflection.
    pub pan_speed: f64,
    /// Radians per second at full deflection.
    pub rotate_speed: f64,
    /// Zoom rate at full deflection, in e-folds per second (1.0 ≈ 2.7× per second).
    pub zoom_speed: f64,
    /// Fraction of full scale ignored around rest.
    pub deadzone: f64,
    /// Response curve exponent: 1 is linear, larger is gentler near rest.
    pub curve: f64,
    /// Inverts axes in the camera frame: pan-x, pan-y, zoom, pitch, yaw, roll.
    pub invert: [bool; 6],
    /// Disables rotation (pan and zoom only).
    pub no_rotate: bool,
    /// Disables pan and zoom (rotation only).
    pub no_translate: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Object,
            pan_speed: 1.0,
            rotate_speed: 2.5,
            zoom_speed: 1.5,
            deadzone: 0.04,
            curve: 1.6,
            invert: [false; 6],
            no_rotate: false,
            no_translate: false,
        }
    }
}

/// The cap's deflection in the camera frame, each component in [-1, 1] after the dead zone and
/// response curve: translation (`t`) and a rotation vector (`w`) that the model should follow.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Input {
    pub t: DVec3,
    pub w: DVec3,
}

impl Input {
    pub fn is_zero(&self) -> bool {
        self.t == DVec3::ZERO && self.w == DVec3::ZERO
    }
}

fn shape(raw: i16, s: &Settings) -> f64 {
    let v = (f64::from(raw) / AXIS_FULL_SCALE).clamp(-1.0, 1.0);
    let mag = v.abs();
    if mag <= s.deadzone {
        return 0.0;
    }
    let scaled = (mag - s.deadzone) / (1.0 - s.deadzone);
    scaled.powf(s.curve).copysign(v)
}

/// Maps raw HID axes (device frame: x right, y toward the user, z down) into the camera frame
/// (x right, y up, z toward the user) so that the model moves and turns the way the cap does.
pub fn input_from_raw(raw: [i16; 6], s: &Settings) -> Input {
    let a: Vec<f64> = raw.iter().map(|&r| shape(r, s)).collect();
    // The change of frame is a proper rotation (x → x, y → z, z → −y), so translations and
    // rotation vectors map the same way.
    let mut v = [a[0], -a[2], a[1], a[3], -a[5], a[4]];
    for (value, inverted) in v.iter_mut().zip(s.invert) {
        if inverted {
            *value = -*value;
        }
    }
    let mut input = Input {
        t: DVec3::new(v[0], v[1], v[2]),
        w: DVec3::new(v[3], v[4], v[5]),
    };
    if s.no_rotate {
        input.w = DVec3::ZERO;
    }
    if s.no_translate {
        input.t = DVec3::ZERO;
    }
    if s.mode == Mode::Camera {
        input.t = -input.t;
        input.w = -input.w;
    }
    input
}

/// How the client lays out `view.affine`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Column-major, translation in elements 12..15 (Onshape).
    ColumnMajor,
    /// Row-major, translation in elements 3, 7, 11 (some other navlib clients).
    RowMajor,
}

/// The camera, as read from the client.
#[derive(Clone, Debug, PartialEq)]
pub struct Camera {
    /// Camera-to-world rotation.
    pub rot: DQuat,
    pub eye: DVec3,
    pub layout: Layout,
    pub perspective: bool,
    pub extents: Option<[f64; 6]>,
    pub frustum: Option<[f64; 6]>,
}

impl Camera {
    /// Reads a camera pose from a flat 16-element `view.affine`.
    pub fn pose_from_affine(a: &[f64]) -> Option<(DQuat, DVec3, Layout)> {
        let m: [f64; 16] = a.get(..16)?.try_into().ok()?;
        if m.iter().any(|v| !v.is_finite()) {
            return None;
        }
        let small = |v: f64| v.abs() < 1e-9;
        let layout = if small(m[3]) && small(m[7]) && small(m[11]) {
            Layout::ColumnMajor
        } else if small(m[12]) && small(m[13]) && small(m[14]) {
            Layout::RowMajor
        } else {
            return None;
        };
        let mat = match layout {
            Layout::ColumnMajor => DMat4::from_cols_array(&m),
            Layout::RowMajor => DMat4::from_cols_array(&m).transpose(),
        };
        let rot = orthonormal_rotation(
            mat.x_axis.truncate(),
            mat.y_axis.truncate(),
            mat.z_axis.truncate(),
        )?;
        Some((rot, mat.w_axis.truncate(), layout))
    }

    /// The flat 16-element `view.affine` for this camera's pose.
    pub fn affine(&self) -> Vec<f64> {
        let r = DMat3::from_quat(self.rot);
        let mat = DMat4::from_cols(
            r.x_axis.extend(0.0),
            r.y_axis.extend(0.0),
            r.z_axis.extend(0.0),
            DVec4::new(self.eye.x, self.eye.y, self.eye.z, 1.0),
        );
        let mat = match self.layout {
            Layout::ColumnMajor => mat,
            Layout::RowMajor => mat.transpose(),
        };
        mat.to_cols_array().to_vec()
    }

    /// The direction the camera looks, in world coordinates.
    pub fn forward(&self) -> DVec3 {
        -(self.rot * DVec3::Z)
    }

    /// Distance from the eye to `point` along the view direction.
    pub fn depth_of(&self, point: DVec3) -> f64 {
        self.forward().dot(point - self.eye)
    }

    /// The height of the view, in world units, at `depth` in front of the eye.
    pub fn view_height(&self, depth: f64) -> f64 {
        if !self.perspective
            && let Some(e) = self.extents
        {
            return (e[4] - e[1]).abs();
        }
        let depth = depth.abs().max(1e-6);
        match self.frustum {
            Some(f) if f[4].abs() > 1e-12 => (f[3] - f[2]).abs() * depth / f[4].abs(),
            // No frustum: assume a 45° vertical field of view.
            _ => 2.0 * depth * (22.5f64).to_radians().tan(),
        }
    }

    /// Advances the camera by one frame of `input` over `dt` seconds, turning about `pivot`.
    pub fn step(&mut self, input: &Input, pivot: DVec3, dt: f64, s: &Settings) {
        let depth = self.depth_of(pivot).abs().max(1e-6);
        let span = self.view_height(depth);

        // Rotation: the model turns by `w` in the camera frame, so the camera turns the
        // opposite way about the pivot.
        let model_turn = DQuat::from_scaled_axis(input.w * s.rotate_speed * dt);
        let camera_turn_world = self.rot * model_turn.inverse() * self.rot.inverse();
        self.eye = pivot + camera_turn_world * (self.eye - pivot);
        self.rot = (camera_turn_world * self.rot).normalize();

        // Pan: the model moves by `t.xy`, so the camera moves the opposite way.
        let pan = input.t.truncate() * s.pan_speed * span * dt;
        let mut camera_move = DVec3::new(-pan.x, -pan.y, 0.0);

        // Zoom: positive `t.z` brings the model closer.
        let zoom = (-input.t.z * s.zoom_speed * dt).exp();
        if self.perspective {
            // Dolly toward the pivot; the distance shrinks geometrically, so it never passes it.
            camera_move.z = -(depth * (1.0 - zoom));
        } else if let Some(e) = self.extents.as_mut() {
            scale_extents(e, zoom);
        }
        self.eye += self.rot * camera_move;
    }

    /// Frames the box `model` (`[minx, miny, minz, maxx, maxy, maxz]`), looking along `rot`.
    pub fn fit(&mut self, model: &[f64; 6], rot: DQuat) {
        let lo = DVec3::new(model[0], model[1], model[2]);
        let hi = DVec3::new(model[3], model[4], model[5]);
        let center = (lo + hi) * 0.5;
        let radius = ((hi - lo).length() * 0.5).max(1e-6) * 1.05;
        let old_depth = self.depth_of(center);
        self.rot = rot.normalize();
        let back = self.rot * DVec3::Z;
        if self.perspective {
            let half = match self.frustum {
                Some(f) if f[4].abs() > 1e-12 => {
                    let near = f[4].abs();
                    ((f[1] - f[0]).abs().min((f[3] - f[2]).abs()) * 0.5 / near).atan()
                }
                _ => (22.5f64).to_radians(),
            };
            self.eye = center + back * (radius / half.sin());
        } else {
            // Orthographic: the distance only matters for clipping; keep it, but stay outside
            // the model.
            self.eye = center + back * old_depth.max(radius * 2.0);
            if let Some(e) = self.extents.as_mut() {
                let aspect = ((e[3] - e[0]) / (e[4] - e[1])).abs();
                let aspect = if aspect.is_finite() && aspect > 0.0 {
                    aspect
                } else {
                    1.0
                };
                let (hx, hy) = if aspect >= 1.0 {
                    (radius * aspect, radius)
                } else {
                    (radius, radius / aspect)
                };
                *e = [-hx, -hy, e[2], hx, hy, e[5]];
            }
        }
    }
}

fn scale_extents(e: &mut [f64; 6], s: f64) {
    let (cx, cy) = ((e[0] + e[3]) * 0.5, (e[1] + e[4]) * 0.5);
    let (hx, hy) = ((e[3] - e[0]) * 0.5 * s, (e[4] - e[1]) * 0.5 * s);
    e[0] = cx - hx;
    e[3] = cx + hx;
    e[1] = cy - hy;
    e[4] = cy + hy;
}

/// Builds a rotation from three (nearly) orthogonal axes, removing scale and skew.
fn orthonormal_rotation(x: DVec3, y: DVec3, z: DVec3) -> Option<DQuat> {
    let z = z.try_normalize()?;
    let x = (x - z * z.dot(x)).try_normalize()?;
    let mut y2 = z.cross(x);
    if y2.dot(y) < 0.0 {
        // Left-handed input; keep the axes the client gave as far as possible.
        y2 = -y2;
    }
    Some(DQuat::from_mat3(&DMat3::from_cols(x, y2, z)).normalize())
}

/// Standard views, as rotations relative to the client's front view (in its camera frame).
pub fn standard_view(name: &str) -> Option<DQuat> {
    use std::f64::consts::{FRAC_PI_2, PI};
    let x = |a: f64| DQuat::from_rotation_x(a);
    let y = |a: f64| DQuat::from_rotation_y(a);
    Some(match name {
        "front" => DQuat::IDENTITY,
        "back" => y(PI),
        "right" => y(FRAC_PI_2),
        "left" => y(-FRAC_PI_2),
        "top" => x(-FRAC_PI_2),
        "bottom" => x(FRAC_PI_2),
        // Front, right and top faces visible.
        "iso" => y(PI / 4.0) * x(-(1.0f64 / 2f64.sqrt()).atan()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn ortho_camera() -> Camera {
        Camera {
            rot: DQuat::IDENTITY,
            eye: DVec3::new(0.0, 0.0, 10.0),
            layout: Layout::ColumnMajor,
            perspective: false,
            extents: Some([-4.0, -3.0, -1000.0, 4.0, 3.0, 1000.0]),
            frustum: None,
        }
    }

    fn linear() -> Settings {
        Settings {
            deadzone: 0.0,
            curve: 1.0,
            ..Settings::default()
        }
    }

    #[test]
    fn affine_round_trips_in_both_layouts() {
        let col = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 2.0, 10.0, 1.0,
        ];
        let (rot, eye, layout) = Camera::pose_from_affine(&col).unwrap();
        assert_eq!(layout, Layout::ColumnMajor);
        assert_eq!(eye, DVec3::new(1.0, 2.0, 10.0));
        let cam = Camera {
            rot,
            eye,
            layout,
            ..ortho_camera()
        };
        assert_eq!(cam.affine(), col.to_vec());

        let row = [
            1.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 2.0, 0.0, 0.0, 1.0, 10.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let (rot, eye, layout) = Camera::pose_from_affine(&row).unwrap();
        assert_eq!(layout, Layout::RowMajor);
        assert_eq!(eye, DVec3::new(1.0, 2.0, 10.0));
        let cam = Camera {
            rot,
            eye,
            layout,
            ..ortho_camera()
        };
        assert_eq!(cam.affine(), row.to_vec());
    }

    #[test]
    fn raw_axes_map_into_the_camera_frame() {
        let s = linear();
        // Cap pushed right → model right.
        assert_eq!(input_from_raw([350, 0, 0, 0, 0, 0], &s).t, DVec3::X);
        // Cap pulled toward the user (+y) → model toward the viewer (+z).
        assert_eq!(input_from_raw([0, 350, 0, 0, 0, 0], &s).t, DVec3::Z);
        // Cap pushed down (+z) → model down (−y).
        assert_eq!(input_from_raw([0, 0, 350, 0, 0, 0], &s).t, -DVec3::Y);
        // Twist (rz) turns about the screen's vertical axis.
        assert_eq!(input_from_raw([0, 0, 0, 0, 0, 350], &s).w, -DVec3::Y);
        let cam = Settings {
            mode: Mode::Camera,
            ..linear()
        };
        assert_eq!(input_from_raw([350, 0, 0, 0, 0, 0], &cam).t, -DVec3::X);
    }

    /// Readings recorded from a SpaceMouse Compact (256f:c635), one motion at a time.
    #[test]
    fn motions_measured_on_a_spacemouse_compact_move_the_model_with_the_cap() {
        let s = Settings::default();
        let dominant = |v: DVec3| {
            let a = v.abs();
            let i = if a.x >= a.y && a.x >= a.z {
                0
            } else if a.y >= a.z {
                1
            } else {
                2
            };
            (i, v[i].signum())
        };
        // Lift: the model goes up.
        assert_eq!(
            dominant(input_from_raw([110, 0, -350, 0, -57, 0], &s).t),
            (1, 1.0)
        );
        // Pull toward you: the model comes closer (zoom in).
        assert_eq!(
            dominant(input_from_raw([36, 350, -39, 33, 29, 0], &s).t),
            (2, 1.0)
        );
        // Twist clockwise seen from above: the model spins clockwise about the screen's vertical.
        assert_eq!(
            dominant(input_from_raw([0, 0, 69, 0, 0, 350], &s).w),
            (1, -1.0)
        );
        // Tilt right: the model rolls clockwise on screen.
        assert_eq!(
            dominant(input_from_raw([248, 0, 149, -38, -299, 2], &s).w),
            (2, -1.0)
        );
        // Tilt back (top toward you): the model's top comes toward you.
        assert_eq!(
            dominant(input_from_raw([0, 239, 42, 294, 48, 6], &s).w),
            (0, 1.0)
        );
    }

    #[test]
    fn dead_zone_and_curve_shape_the_input() {
        let s = Settings {
            deadzone: 0.1,
            curve: 2.0,
            ..Settings::default()
        };
        assert_eq!(shape(30, &s), 0.0);
        assert!(close(shape(350, &s), 1.0));
        assert!(close(shape(-350, &s), -1.0));
        let half = shape(((0.1 + 0.9 * 0.5) * 350.0) as i16, &s);
        assert!((half - 0.25).abs() < 0.01);
    }

    #[test]
    fn panning_right_moves_the_camera_left_by_view_heights() {
        let mut cam = ortho_camera();
        let input = Input {
            t: DVec3::X,
            w: DVec3::ZERO,
        };
        cam.step(&input, DVec3::ZERO, 0.5, &linear());
        // View height 6, 1 view height per second, half a second: the model moves 3 right.
        assert!(close(cam.eye.x, -3.0));
        assert!(close(cam.eye.z, 10.0));
    }

    #[test]
    fn orthographic_zoom_scales_extents() {
        let mut cam = ortho_camera();
        let s = Settings {
            zoom_speed: 1.0,
            ..linear()
        };
        cam.step(
            &Input {
                t: DVec3::Z,
                w: DVec3::ZERO,
            },
            DVec3::ZERO,
            1.0,
            &s,
        );
        let e = cam.extents.unwrap();
        assert!(close(e[4], 3.0 * (-1.0f64).exp()));
        assert!(close(e[2], -1000.0), "clip planes untouched");
        assert_eq!(cam.eye, DVec3::new(0.0, 0.0, 10.0));
    }

    #[test]
    fn perspective_zoom_dollies_toward_the_pivot_without_passing_it() {
        let mut cam = Camera {
            perspective: true,
            extents: None,
            ..ortho_camera()
        };
        let s = Settings {
            zoom_speed: 1.0,
            ..linear()
        };
        for _ in 0..100 {
            cam.step(
                &Input {
                    t: DVec3::Z,
                    w: DVec3::ZERO,
                },
                DVec3::ZERO,
                0.1,
                &s,
            );
        }
        assert!(cam.eye.z > 0.0 && cam.eye.z < 0.01);
    }

    #[test]
    fn rotation_orbits_the_pivot_opposite_to_the_model() {
        let mut cam = ortho_camera();
        let s = Settings {
            rotate_speed: std::f64::consts::FRAC_PI_2,
            ..linear()
        };
        // The model turns +90° about the screen's vertical axis (its front swings to the right),
        // so the camera swings to the model's left: from +z to −x.
        cam.step(
            &Input {
                t: DVec3::ZERO,
                w: DVec3::Y,
            },
            DVec3::ZERO,
            1.0,
            &s,
        );
        assert!(
            close(cam.eye.x, -10.0) && close(cam.eye.z, 0.0),
            "{:?}",
            cam.eye
        );
        assert!(
            (cam.forward() - DVec3::X).length() < 1e-9,
            "still looks at the pivot"
        );
    }

    #[test]
    fn fit_frames_the_model() {
        let mut cam = ortho_camera();
        cam.fit(&[-1.0, -1.0, -1.0, 1.0, 1.0, 1.0], DQuat::IDENTITY);
        let e = cam.extents.unwrap();
        let r = 3f64.sqrt() * 1.05;
        assert!(close(e[4], r) && close(e[3], r * 4.0 / 3.0));
        let mut persp = Camera {
            perspective: true,
            extents: None,
            frustum: Some([-1.0, 1.0, -1.0, 1.0, 1.0, 100.0]),
            ..ortho_camera()
        };
        persp.fit(
            &[-1.0, -1.0, -1.0, 1.0, 1.0, 1.0],
            standard_view("top").unwrap(),
        );
        // Looking down −y from above the box.
        assert!((persp.forward() + DVec3::Y).length() < 1e-9);
        assert!(close(persp.eye.y, r / (45f64.to_radians()).sin()));
    }
}
