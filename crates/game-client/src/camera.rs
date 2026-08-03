use bevy::{
    camera::ScalingMode,
    input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseScrollUnit},
    picking::pointer::PointerInteraction,
    prelude::*,
};

#[derive(Component)]
pub struct GameCamera;

#[derive(Component, Debug)]
pub struct CameraRig {
    pub focus: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

impl Default for CameraRig {
    fn default() -> Self {
        Self {
            focus: Vec3::new(0.0, 0.45, 0.0),
            yaw: -std::f32::consts::FRAC_PI_4,
            pitch: -0.92,
            distance: 42.0,
        }
    }
}

pub fn spawn_camera_and_light(mut commands: Commands) {
    let rig = CameraRig::default();
    let transform = rig_transform(&rig);
    commands.spawn((
        Name::new("RTS camera"),
        GameCamera,
        Camera3d::default(),
        Projection::from(OrthographicProjection {
            scaling_mode: ScalingMode::FixedVertical {
                viewport_height: 29.0,
            },
            ..OrthographicProjection::default_3d()
        }),
        transform,
        rig,
    ));

    commands.spawn((
        Name::new("Graybox sun"),
        DirectionalLight {
            color: Color::srgb(1.0, 0.92, 0.78),
            illuminance: 13_500.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(-14.0, 24.0, 11.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

#[allow(clippy::too_many_arguments)]
pub fn camera_controls(
    time: Res<Time>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    mouse_motion: Res<AccumulatedMouseMotion>,
    mouse_scroll: Res<AccumulatedMouseScroll>,
    pointers: Query<&PointerInteraction>,
    ui_nodes: Query<(), With<Node>>,
    camera: Single<(&mut CameraRig, &mut Transform, &mut Projection), With<GameCamera>>,
) {
    let pointer_over_ui = pointers.iter().any(|interaction| {
        interaction
            .get_nearest_hit()
            .is_some_and(|(entity, _)| ui_nodes.contains(*entity))
    });
    let (mut rig, mut transform, mut projection) = camera.into_inner();

    if keyboard.just_pressed(KeyCode::Home) {
        *rig = CameraRig::default();
        if let Projection::Orthographic(orthographic) = &mut *projection {
            orthographic.scale = 1.0;
        }
    }

    let rotation_axis =
        f32::from(keyboard.pressed(KeyCode::KeyE)) - f32::from(keyboard.pressed(KeyCode::KeyQ));
    rig.yaw += rotation_axis * 1.25 * time.delta_secs();

    let (forward, right) = planar_camera_axes(rig.yaw);
    let mut keyboard_pan = Vec3::ZERO;
    if keyboard.pressed(KeyCode::KeyW) {
        keyboard_pan += forward;
    }
    if keyboard.pressed(KeyCode::KeyS) {
        keyboard_pan -= forward;
    }
    if keyboard.pressed(KeyCode::KeyD) {
        keyboard_pan += right;
    }
    if keyboard.pressed(KeyCode::KeyA) {
        keyboard_pan -= right;
    }

    let zoom_scale = match &*projection {
        Projection::Orthographic(orthographic) => orthographic.scale,
        _ => 1.0,
    };
    rig.focus += keyboard_pan.normalize_or_zero() * 13.0 * zoom_scale * time.delta_secs();

    let drag_pan = mouse_buttons.pressed(MouseButton::Middle)
        || (keyboard.pressed(KeyCode::Space) && mouse_buttons.pressed(MouseButton::Left));
    if drag_pan && !pointer_over_ui {
        rig.focus +=
            (-right * mouse_motion.delta.x + forward * mouse_motion.delta.y) * 0.030 * zoom_scale;
    }

    if !pointer_over_ui
        && mouse_scroll.delta.y != 0.0
        && let Projection::Orthographic(orthographic) = &mut *projection
    {
        let sensitivity = match mouse_scroll.unit {
            MouseScrollUnit::Line => 0.115,
            MouseScrollUnit::Pixel => 0.0022,
        };
        orthographic.scale =
            (orthographic.scale * (-mouse_scroll.delta.y * sensitivity).exp()).clamp(0.28, 4.8);
    }

    *transform = rig_transform(&rig);
}

fn rig_transform(rig: &CameraRig) -> Transform {
    let horizontal = rig.pitch.cos() * rig.distance;
    let offset = Vec3::new(
        rig.yaw.sin() * horizontal,
        -rig.pitch.sin() * rig.distance,
        rig.yaw.cos() * horizontal,
    );
    Transform::from_translation(rig.focus + offset).looking_at(rig.focus, Vec3::Y)
}

fn planar_camera_axes(yaw: f32) -> (Vec3, Vec3) {
    let forward = Vec3::new(-yaw.sin(), 0.0, -yaw.cos()).normalize_or_zero();
    let right = forward.cross(Vec3::Y).normalize_or_zero();
    (forward, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f32 = 1.0e-6;

    #[test]
    fn camera_at_zero_yaw_uses_positive_x_as_screen_right() {
        let (forward, right) = planar_camera_axes(0.0);

        assert!((forward - Vec3::NEG_Z).length() < EPSILON);
        assert!((right - Vec3::X).length() < EPSILON);
    }

    #[test]
    fn default_camera_axes_are_orthonormal_and_face_the_expected_diagonal() {
        let rig = CameraRig::default();
        let (forward, right) = planar_camera_axes(rig.yaw);
        let diagonal = std::f32::consts::FRAC_1_SQRT_2;

        assert!((forward - Vec3::new(diagonal, 0.0, -diagonal)).length() < EPSILON);
        assert!((right - Vec3::new(diagonal, 0.0, diagonal)).length() < EPSILON);
        assert!(forward.dot(right).abs() < EPSILON);
        assert!((forward.length() - 1.0).abs() < EPSILON);
        assert!((right.length() - 1.0).abs() < EPSILON);
    }
}
