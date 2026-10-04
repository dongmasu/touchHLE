//! App picker GUI.
//!
//! This also includes a license text viewer. The license text viewer is needed
//! on Android, where the command-line way to view license text doesn't exist.

use crate::bundle::Bundle;
use crate::frameworks::core_graphics::cg_bitmap_context::{
    CGBitmapContextCreate, CGBitmapContextCreateImage,
};
use crate::frameworks::core_graphics::cg_color_space::CGColorSpaceCreateDeviceRGB;
use crate::frameworks::core_graphics::cg_context::{
    CGContextFillRect, CGContextRelease, CGContextScaleCTM, CGContextSetRGBFillColor,
    CGContextTranslateCTM,
};
use crate::frameworks::core_graphics::cg_image::{self, kCGImageAlphaPremultipliedLast};
use crate::frameworks::core_graphics::{CGFloat, CGPoint, CGRect, CGSize};
use crate::frameworks::foundation::ns_run_loop::run_run_loop_single_iteration;
use crate::frameworks::foundation::ns_string;
use crate::frameworks::uikit::ui_font::{
    UITextAlignmentCenter, UITextAlignmentLeft, UITextAlignmentRight,
};
use crate::frameworks::uikit::ui_graphics::{UIGraphicsPopContext, UIGraphicsPushContext};
use crate::frameworks::uikit::ui_view::ui_control::ui_button::{
    UIButtonTypeCustom, UIButtonTypeRoundedRect,
};
use crate::frameworks::uikit::ui_view::ui_control::ui_switch::TOTAL_WIDTH as UI_SWITCH_WIDTH;
use crate::frameworks::uikit::ui_view::ui_control::{
    UIControlEventTouchUpInside, UIControlEventValueChanged, UIControlStateNormal,
};
use crate::fs::BundleData;
use crate::image::Image;
use crate::mem::Ptr;
use crate::objc::{id, msg, msg_class, nil, objc_classes, release, ClassExports, HostObject};
use crate::options::Options;
use crate::paths;
use crate::sync::auth::{self, PendingAuthorization};
use crate::sync::coordinator::{load_settings, save_settings, settings_path, SyncSettings};
use crate::sync::live::{affects_apps, LiveControl, LiveEvent};
use crate::sync::model::{LocalOrRemote, SnapshotEntry, SyncError};
use crate::sync::reconcile::{ConflictChoice, RemoteVersionId, SyncPlan};
use crate::sync::status::{load_live_status, LiveStatus};
use crate::window::DeviceOrientation;
use crate::Environment;
use chrono::{Local, TimeZone};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct AppInfo {
    path: PathBuf,
    display_name: String,
    icon: Option<Image>,
    /// `NSString*`
    display_name_ns_string: Option<id>,
    /// `UIImage*`
    icon_ui_image: Option<id>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerSyncResult {
    Completed,
    Offline,
}

pub fn app_picker<F>(
    options: Options,
    live_control: Option<LiveControl>,
    run_sync: F,
) -> Result<Option<(PathBuf, Vec<String>)>, String>
where
    F: FnMut() -> Result<(Option<LiveControl>, Result<PickerSyncResult, String>), String> + 'static,
{
    let sync_root = paths::user_data_base_path().to_path_buf();
    let apps_dir = sync_root.join(paths::APPS_DIR);
    let apps = refresh_apps(&apps_dir);
    show_app_picker_gui(options, sync_root, apps, live_control, run_sync)
}

fn refresh_apps(apps_dir: &Path) -> Result<Vec<AppInfo>, String> {
    match std::fs::metadata(apps_dir) {
        Ok(metadata) if metadata.is_dir() => enumerate_apps(apps_dir).map_err(|error| {
            format!(
                "Couldn't get list of apps in the {} directory: {error}.",
                apps_dir.display()
            )
        }),
        Ok(_) => Err(format!(
            "The {} path exists but is not a directory.",
            apps_dir.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!(
            "Couldn't access the {} directory: {error}.",
            apps_dir.display()
        )),
    }
}

fn picker_icon() -> Image {
    let bytes: &[u8] = match crate::branding() {
        "" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/res/icon.png")),
        "UNOFFICIAL" => include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/res/icon_unofficial.png"
        )),
        "PREVIEW" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/res/icon_preview.png")),
        _ => panic!(),
    };
    let mut image = Image::from_bytes(bytes).unwrap();
    image.round_corners(
        (10.0 / 57.0) * (image.dimensions().0 as f32),
        /* four_corners: */ true,
        /* add_sheen: */ true,
    );
    image
}

fn picker_rect_to_portrait(
    rect: CGRect,
    orientation: DeviceOrientation,
    portrait_size: (u32, u32),
) -> CGRect {
    let (width, height) = (portrait_size.0 as CGFloat, portrait_size.1 as CGFloat);
    match orientation {
        DeviceOrientation::Portrait => rect,
        DeviceOrientation::PortraitUpsideDown => CGRect {
            origin: CGPoint {
                x: width - rect.origin.x - rect.size.width,
                y: height - rect.origin.y - rect.size.height,
            },
            size: rect.size,
        },
        DeviceOrientation::LandscapeLeft => CGRect {
            origin: CGPoint {
                x: rect.origin.y,
                y: height - rect.origin.x - rect.size.width,
            },
            size: CGSize {
                width: rect.size.height,
                height: rect.size.width,
            },
        },
        DeviceOrientation::LandscapeRight => CGRect {
            origin: CGPoint {
                x: width - rect.origin.y - rect.size.height,
                y: rect.origin.x,
            },
            size: CGSize {
                width: rect.size.height,
                height: rect.size.width,
            },
        },
    }
}

fn picker_rect_from_portrait(
    rect: CGRect,
    orientation: DeviceOrientation,
    portrait_size: (u32, u32),
) -> CGRect {
    let (width, height) = (portrait_size.0 as CGFloat, portrait_size.1 as CGFloat);
    match orientation {
        DeviceOrientation::Portrait => rect,
        DeviceOrientation::PortraitUpsideDown => CGRect {
            origin: CGPoint {
                x: width - rect.origin.x - rect.size.width,
                y: height - rect.origin.y - rect.size.height,
            },
            size: rect.size,
        },
        DeviceOrientation::LandscapeLeft => CGRect {
            origin: CGPoint {
                x: height - rect.origin.y - rect.size.height,
                y: rect.origin.x,
            },
            size: CGSize {
                width: rect.size.height,
                height: rect.size.width,
            },
        },
        DeviceOrientation::LandscapeRight => CGRect {
            origin: CGPoint {
                x: rect.origin.y,
                y: width - rect.origin.x - rect.size.width,
            },
            size: CGSize {
                width: rect.size.height,
                height: rect.size.width,
            },
        },
    }
}

#[derive(Clone, Copy)]
struct PickerViewFrame {
    view: id,
    parent: id,
    frame_in_window: CGRect,
}

fn collect_picker_view_frames(
    env: &mut Environment,
    parent: id,
    parent_origin: CGPoint,
    frames: &mut Vec<PickerViewFrame>,
) {
    let subviews: id = msg![env; parent subviews];
    let count: crate::frameworks::foundation::NSUInteger = msg![env; subviews count];
    for index in 0..count {
        let view: id = msg![env; subviews objectAtIndex:index];
        let frame: CGRect = msg![env; view frame];
        let frame_in_window = CGRect {
            origin: CGPoint {
                x: parent_origin.x + frame.origin.x,
                y: parent_origin.y + frame.origin.y,
            },
            size: frame.size,
        };
        frames.push(PickerViewFrame {
            view,
            parent,
            frame_in_window,
        });
        collect_picker_view_frames(env, view, frame_in_window.origin, frames);
    }
}

fn reflow_picker_views(
    env: &mut Environment,
    screen: id,
    window: id,
    main_view: id,
    old_orientation: DeviceOrientation,
    new_orientation: DeviceOrientation,
    portrait_size: (u32, u32),
) -> CGRect {
    let old_root_frame: CGRect = msg![env; main_view frame];
    let mut frames = Vec::new();
    collect_picker_view_frames(env, main_view, old_root_frame.origin, &mut frames);

    let screen_bounds: CGRect = msg![env; screen bounds];
    let app_frame: CGRect = msg![env; screen applicationFrame];
    () = msg![env; window setFrame:screen_bounds];
    () = msg![env; main_view setFrame:app_frame];

    let mut frames_in_new_window = HashMap::from([(main_view, app_frame)]);
    for PickerViewFrame {
        view,
        parent,
        frame_in_window,
    } in frames.iter().copied()
    {
        let frame_in_portrait =
            picker_rect_to_portrait(frame_in_window, old_orientation, portrait_size);
        let new_frame_in_window =
            picker_rect_from_portrait(frame_in_portrait, new_orientation, portrait_size);
        let parent_frame = frames_in_new_window[&parent];
        let frame_in_parent = CGRect {
            origin: CGPoint {
                x: new_frame_in_window.origin.x - parent_frame.origin.x,
                y: new_frame_in_window.origin.y - parent_frame.origin.y,
            },
            size: new_frame_in_window.size,
        };
        () = msg![env; view setFrame:frame_in_parent];
        frames_in_new_window.insert(view, new_frame_in_window);
    }
    for PickerViewFrame { view, .. } in frames {
        () = msg![env; view layoutSubviews];
    }
    app_frame
}

#[cfg(test)]
mod picker_rotation_tests {
    use super::*;

    #[test]
    fn view_frames_round_trip_through_each_device_orientation() {
        let frame = CGRect {
            origin: CGPoint { x: 37.0, y: 91.0 },
            size: CGSize {
                width: 80.0,
                height: 34.0,
            },
        };
        let portrait_size = (320, 480);
        for orientation in [
            DeviceOrientation::Portrait,
            DeviceOrientation::PortraitUpsideDown,
            DeviceOrientation::LandscapeLeft,
            DeviceOrientation::LandscapeRight,
        ] {
            let rotated = picker_rect_from_portrait(frame, orientation, portrait_size);
            let restored = picker_rect_to_portrait(rotated, orientation, portrait_size);
            assert_eq!(restored, frame, "orientation {orientation:?}");
        }
    }

    #[test]
    fn landscape_picker_frames_use_landscape_dimensions() {
        let frame = CGRect {
            origin: CGPoint { x: 10.0, y: 20.0 },
            size: CGSize {
                width: 60.0,
                height: 30.0,
            },
        };
        let landscape =
            picker_rect_from_portrait(frame, DeviceOrientation::LandscapeLeft, (320, 480));
        assert_eq!(landscape.origin, CGPoint { x: 430.0, y: 10.0 });
        assert_eq!(
            landscape.size,
            CGSize {
                width: 30.0,
                height: 60.0
            }
        );
    }

    #[test]
    fn footer_buttons_stay_horizontal_and_inside_portrait_and_landscape_frames() {
        for (size, divider, expected_width, expected_y) in [
            (
                CGSize {
                    width: 320.0,
                    height: 460.0,
                },
                360.0,
                145.0,
                388.0,
            ),
            (
                CGSize {
                    width: 480.0,
                    height: 300.0,
                },
                200.0,
                225.0,
                228.0,
            ),
        ] {
            let frames = footer_button_frames(size, divider, 2);
            assert_eq!(frames.len(), 2);
            for frame in &frames {
                let x = frame.origin.x;
                let y = frame.origin.y;
                let width = frame.size.width;
                let height = frame.size.height;
                assert_eq!(width, expected_width);
                assert_eq!(height, 44.0);
                assert_eq!(y, expected_y);
                assert!(x >= 0.0);
                assert!(x + width <= size.width);
                assert!(y + height <= size.height);
            }
        }
    }

    #[test]
    fn more_actions_modal_covers_the_picker_and_centers_in_each_orientation() {
        let portrait = more_actions_panel_frame(
            CGSize {
                width: 320.0,
                height: 480.0,
            },
            DeviceOrientation::Portrait,
        );
        assert_eq!(portrait.origin, CGPoint { x: 8.0, y: 125.0 });
        assert_eq!(
            portrait.size,
            CGSize {
                width: 304.0,
                height: MORE_ACTIONS_PANEL_HEIGHT,
            }
        );

        for orientation in [
            DeviceOrientation::LandscapeLeft,
            DeviceOrientation::LandscapeRight,
        ] {
            let landscape = more_actions_panel_frame(
                CGSize {
                    width: 480.0,
                    height: 320.0,
                },
                orientation,
            );
            assert_eq!(landscape.origin, CGPoint { x: 100.0, y: 45.0 });
            assert_eq!(
                landscape.size,
                CGSize {
                    width: MORE_ACTIONS_LANDSCAPE_WIDTH,
                    height: MORE_ACTIONS_PANEL_HEIGHT,
                }
            );
            assert!(landscape.origin.x >= 0.0);
            assert!(landscape.origin.y >= 0.0);
            assert!(landscape.origin.x + landscape.size.width <= 480.0);
            assert!(landscape.origin.y + landscape.size.height <= 320.0);
        }
    }
}

pub fn resolve_conflicts_gui(
    options: Options,
    plan: &SyncPlan,
) -> Result<Option<Vec<ConflictChoice>>, String> {
    if plan.conflicts.is_empty() {
        return Ok(Some(Vec::new()));
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = options;
        return crate::window::resolve_sync_conflicts(plan);
    }
    #[cfg(target_os = "android")]
    {
        let environment = Environment::new_without_app(options, picker_icon())?;
        let plan = plan.clone();
        Ok(environment.run_app_picker(move |env| resolve_conflicts_inner(env, &plan)))
    }
}

fn resolve_conflicts_inner(env: &mut Environment, plan: &SyncPlan) -> Option<Vec<ConflictChoice>> {
    let ui_application: id = msg_class![env; UIApplication new];
    let delegate_class = env
        .objc
        .get_known_class("_touchHLE_AppPickerDelegate", &mut env.mem);
    let delegate = env.objc.alloc_object(
        delegate_class,
        Box::<AppPickerDelegateHostObject>::default(),
        &mut env.mem,
    );
    () = msg![env; ui_application setDelegate:delegate];

    let screen: id = msg_class![env; UIScreen mainScreen];
    let picker_auto_rotate = env.window().is_picker_auto_rotating();
    let mut picker_orientation = env.window().current_rotation();
    let picker_portrait_size = env
        .window()
        .logical_screen_size(DeviceOrientation::Portrait);
    let bounds: CGRect = if picker_auto_rotate {
        CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: picker_portrait_size.0 as CGFloat,
                height: picker_portrait_size.1 as CGFloat,
            },
        }
    } else {
        msg![env; screen bounds]
    };
    let frame: CGRect = if picker_auto_rotate {
        let status_bar_hidden: bool = msg![env; ui_application isStatusBarHidden];
        let status_bar_height = if status_bar_hidden { 0.0 } else { 20.0 };
        CGRect {
            origin: CGPoint {
                x: 0.0,
                y: status_bar_height,
            },
            size: CGSize {
                width: bounds.size.width,
                height: bounds.size.height - status_bar_height,
            },
        }
    } else {
        msg![env; screen applicationFrame]
    };
    let window: id = msg_class![env; UIWindow alloc];
    let window: id = msg![env; window initWithFrame:bounds];
    let root: id = msg_class![env; UIView alloc];
    let root: id = msg![env; root initWithFrame:frame];
    let background: id = msg_class![env; UIColor whiteColor];
    () = msg![env; root setBackgroundColor:background];
    () = msg![env; window addSubview:root];

    let title_frame = CGRect {
        origin: CGPoint { x: 12.0, y: 12.0 },
        size: CGSize {
            width: frame.size.width - 24.0,
            height: 34.0,
        },
    };
    let title: id = msg_class![env; UILabel alloc];
    let title: id = msg![env; title initWithFrame:title_frame];
    let title_text = ns_string::get_static_str(env, "Resolve Google Drive sync conflicts");
    () = msg![env; title setText:title_text];
    let title_font: id = msg_class![env; UIFont boldSystemFontOfSize:(18.0 as CGFloat)];
    () = msg![env; title setFont:title_font];
    () = msg![env; root addSubview:title];

    let detail_frame = CGRect {
        origin: CGPoint { x: 12.0, y: 55.0 },
        size: CGSize {
            width: frame.size.width - 24.0,
            height: frame.size.height - 225.0,
        },
    };
    let detail: id = msg_class![env; UILabel alloc];
    let detail: id = msg![env; detail initWithFrame:detail_frame];
    () = msg![env; detail setNumberOfLines:0];
    let detail_font: id = msg_class![env; UIFont systemFontOfSize:(15.0 as CGFloat)];
    () = msg![env; detail setFont:detail_font];
    let text_color: id = msg_class![env; UIColor blackColor];
    () = msg![env; detail setTextColor:text_color];
    () = msg![env; root addSubview:detail];

    let status_frame = CGRect {
        origin: CGPoint {
            x: 12.0,
            y: frame.size.height - 170.0,
        },
        size: CGSize {
            width: frame.size.width - 24.0,
            height: 25.0,
        },
    };
    let status: id = msg_class![env; UILabel alloc];
    let status: id = msg![env; status initWithFrame:status_frame];
    () = msg![env; status setNumberOfLines:0];
    let status_font: id = msg_class![env; UIFont systemFontOfSize:(13.0 as CGFloat)];
    () = msg![env; status setFont:status_font];
    () = msg![env; root addSubview:status];

    let action_rows = [
        (
            frame.size.height - 126.0,
            [
                ("Version -", "syncAction:"),
                ("Use version", "syncAction:"),
                ("Version +", "syncAction:"),
            ]
            .as_slice(),
        ),
        (
            frame.size.height - 84.0,
            [
                ("Previous file", "syncAction:"),
                ("Next file", "syncAction:"),
            ]
            .as_slice(),
        ),
        (
            frame.size.height - 42.0,
            [("Apply all", "syncAction:"), ("Cancel", "syncAction:")].as_slice(),
        ),
    ];
    let row_actions = [
        [
            SyncResolverAction::PreviousVersion,
            SyncResolverAction::ChooseVersion,
            SyncResolverAction::NextVersion,
        ]
        .as_slice(),
        [
            SyncResolverAction::PreviousFile,
            SyncResolverAction::NextFile,
        ]
        .as_slice(),
        [SyncResolverAction::Apply, SyncResolverAction::Cancel].as_slice(),
    ];
    for ((row_center, buttons), actions) in action_rows.iter().zip(row_actions) {
        let row = make_button_row(
            env,
            delegate,
            root,
            frame.size,
            *row_center,
            buttons,
            Some(13.0),
        );
        for (button, action) in row.into_iter().zip(actions) {
            let tag: i32 = match action {
                SyncResolverAction::PreviousFile => 0,
                SyncResolverAction::NextFile => 1,
                SyncResolverAction::PreviousVersion => 2,
                SyncResolverAction::NextVersion => 3,
                SyncResolverAction::ChooseVersion => 4,
                SyncResolverAction::Apply => 5,
                SyncResolverAction::Cancel => 6,
            };
            () = msg![env; button setTag:tag];
        }
    }

    if picker_auto_rotate && picker_orientation != DeviceOrientation::Portrait {
        reflow_picker_views(
            env,
            screen,
            window,
            root,
            DeviceOrientation::Portrait,
            picker_orientation,
            picker_portrait_size,
        );
    }
    () = msg![env; window makeKeyAndVisible];
    let main_run_loop: id = msg_class![env; NSRunLoop mainRunLoop];
    let mut conflict_idx = 0usize;
    let mut version_idx = 0usize;
    let mut selected = vec![None; plan.conflicts.len()];
    let mut status_text = "Choose one local or cloud version for every path.".to_owned();

    loop {
        update_conflict_labels(
            env,
            detail,
            status,
            plan,
            conflict_idx,
            version_idx,
            &selected,
            &status_text,
        );
        run_run_loop_single_iteration(env, main_run_loop);
        if env.exit_requested() {
            return None;
        }
        let current_orientation = env.window().current_rotation();
        if picker_auto_rotate && current_orientation != picker_orientation {
            reflow_picker_views(
                env,
                screen,
                window,
                root,
                picker_orientation,
                current_orientation,
                picker_portrait_size,
            );
            picker_orientation = current_orientation;
        }
        let action = env
            .objc
            .borrow_mut::<AppPickerDelegateHostObject>(delegate)
            .sync_action
            .take();
        let Some(action) = action else {
            continue;
        };

        let candidate_count = plan.conflicts[conflict_idx].remote_candidates.len() + 1;
        match action {
            SyncResolverAction::PreviousFile => {
                conflict_idx = conflict_idx.saturating_sub(1);
                version_idx = selected[conflict_idx].unwrap_or(0);
                status_text.clear();
            }
            SyncResolverAction::NextFile => {
                conflict_idx = (conflict_idx + 1).min(plan.conflicts.len() - 1);
                version_idx = selected[conflict_idx].unwrap_or(0);
                status_text.clear();
            }
            SyncResolverAction::PreviousVersion => {
                version_idx = (version_idx + candidate_count - 1) % candidate_count;
                status_text.clear();
            }
            SyncResolverAction::NextVersion => {
                version_idx = (version_idx + 1) % candidate_count;
                status_text.clear();
            }
            SyncResolverAction::ChooseVersion => {
                selected[conflict_idx] = Some(version_idx);
                status_text = "Choice saved for this path.".to_owned();
            }
            SyncResolverAction::Apply => {
                if selected.iter().any(Option::is_none) {
                    status_text = "Choose a version for every path before applying.".to_owned();
                    continue;
                }
                let choices = match build_conflict_choices(plan, &selected) {
                    Ok(choices) => choices,
                    Err(error) => {
                        status_text = crate::sync::status::redacted_error(&error);
                        continue;
                    }
                };
                match plan.resolved_snapshot(&choices) {
                    Ok(_) => return Some(choices),
                    Err(error) => {
                        status_text = format!(
                            "These choices make an incompatible file tree: {}. Adjust a choice and retry.",
                            crate::sync::status::redacted_error(&error)
                        );
                    }
                }
            }
            SyncResolverAction::Cancel => return None,
        }
    }
}

fn update_conflict_labels(
    env: &mut Environment,
    detail: id,
    status: id,
    plan: &SyncPlan,
    conflict_idx: usize,
    version_idx: usize,
    selected: &[Option<usize>],
    status_text: &str,
) {
    let conflict = &plan.conflicts[conflict_idx];
    let mut text = format!(
        "File {} of {}\n{}\n\nLocal: {}\n",
        conflict_idx + 1,
        plan.conflicts.len(),
        conflict.path.as_str(),
        describe_sync_version(conflict.local.as_ref()),
    );
    for (index, candidate) in conflict.remote_candidates.iter().enumerate() {
        let source = match &candidate.id {
            RemoteVersionId::DriveFile(id) => format!(
                "Drive file {id}, version {}",
                candidate
                    .file
                    .as_ref()
                    .map_or("unknown", |file| &file.version)
            ),
            RemoteVersionId::LegacyCommit(id) => format!("legacy revision {id}"),
            RemoteVersionId::NoDriveFile => "Drive file deleted".to_owned(),
        };
        text.push_str(&format!(
            "Cloud {}: {} ({}){}\n",
            index + 1,
            describe_sync_version(candidate.entry.as_ref()),
            source,
            if version_idx == index + 1 {
                "  < current"
            } else {
                ""
            }
        ));
    }
    text.push_str(&format!(
        "\nSelected for this path: {}",
        selected[conflict_idx].map_or("not selected".to_owned(), |choice| format!(
            "version {}",
            choice + 1
        ))
    ));
    text.push_str(&format!(
        "\nCurrent choice: {}",
        if version_idx == 0 {
            "Local".to_owned()
        } else {
            format!("Cloud {}", version_idx)
        }
    ));
    let value = ns_string::from_rust_string(env, text);
    () = msg![env; detail setText:value];
    let status_value = ns_string::from_rust_string(env, status_text.to_owned());
    () = msg![env; status setText:status_value];
}

fn describe_sync_version(entry: Option<&SnapshotEntry>) -> String {
    match entry {
        None | Some(SnapshotEntry::Tombstone) => "deleted".to_owned(),
        Some(SnapshotEntry::File {
            size,
            modified_unix_ms,
            ..
        }) => format!(
            "{size} bytes, modified {}",
            format_unix_ms_local(*modified_unix_ms)
        ),
    }
}

pub(crate) fn format_unix_ms_local(timestamp_ms: i64) -> String {
    Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M:%S%.3f %:z").to_string())
        .unwrap_or_else(|| format!("Invalid timestamp ({timestamp_ms} ms)"))
}

fn build_conflict_choices(
    plan: &SyncPlan,
    selected: &[Option<usize>],
) -> Result<Vec<ConflictChoice>, SyncError> {
    plan.conflicts
        .iter()
        .zip(selected)
        .map(|(conflict, selected)| {
            let selected = selected.ok_or_else(|| {
                SyncError::UnresolvedConflicts(format!(
                    "no version selected for {}",
                    conflict.path.as_str()
                ))
            })?;
            if selected == 0 {
                Ok(ConflictChoice {
                    path: conflict.path.clone(),
                    selected: LocalOrRemote::Local,
                    remote_version_id: None,
                })
            } else {
                let candidate = conflict
                    .remote_candidates
                    .get(selected - 1)
                    .ok_or_else(|| {
                        SyncError::UnresolvedConflicts(format!(
                            "cloud version {} is no longer available for {}",
                            selected,
                            conflict.path.as_str()
                        ))
                    })?;
                Ok(ConflictChoice {
                    path: conflict.path.clone(),
                    selected: LocalOrRemote::Remote,
                    remote_version_id: Some(candidate.id.clone()),
                })
            }
        })
        .collect()
}

fn enumerate_apps(apps_dir: &Path) -> Result<Vec<AppInfo>, std::io::Error> {
    let mut apps = Vec::new();
    for app in std::fs::read_dir(apps_dir)? {
        let app_path = app?.path();
        if app_path.extension() != Some(OsStr::new("app"))
            && app_path.extension() != Some(OsStr::new("ipa"))
        {
            continue;
        }

        // TODO: avoid loading the whole FS somehow?
        let (bundle, fs) = match BundleData::open_any(&app_path).and_then(|bundle_data| {
            Bundle::new_bundle_and_fs_from_host_path(bundle_data, /* read_only_mode: */ true)
        }) {
            Ok(ok) => ok,
            Err(e) => {
                log!(
                    "Warning: couldn't open app bundle {}: {} (skipping)",
                    app_path.display(),
                    e
                );
                continue;
            }
        };

        // TODO: what if this crashes?
        let display_name = bundle.display_name().to_owned();

        let icon = match bundle.load_icon(&fs) {
            Ok(icon) => Some(icon),
            Err(e) => {
                log!("Warning: couldn't load icon for app bundle {}: {} (displaying placeholder instead)", app_path.display(), e);
                None
            }
        };

        apps.push(AppInfo {
            path: app_path,
            display_name,
            icon,
            display_name_ns_string: None,
            icon_ui_image: None,
        });
    }

    apps.sort_by_key(|app| app.display_name.to_uppercase());

    Ok(apps)
}

#[derive(Default)]
struct AppPickerDelegateHostObject {
    icon_tapped: id,
    copyright_show: bool,
    copyright_hide: bool,
    copyright_prev: bool,
    copyright_next: bool,
    quick_options_show: bool,
    quick_options_hide: bool,
    scale_hack_default: bool,
    scale_hack1: bool,
    scale_hack2: bool,
    scale_hack3: bool,
    scale_hack4: bool,
    orientation_default: bool,
    orientation_portrait_upside_down: bool,
    orientation_landscape_left: bool,
    orientation_landscape_right: bool,
    analog_stick_tilt_controls: Option<bool>,
    network: Option<bool>,
    fullscreen: Option<bool>,
    sync_action: Option<SyncResolverAction>,
    sync_settings_show: bool,
    sync_settings_hide: bool,
    sync_toggle: Option<bool>,
    sync_connect: bool,
    more_actions_toggle: bool,
    more_actions_dismiss: bool,
}
impl HostObject for AppPickerDelegateHostObject {}

fn take_picker_host_value<T>(
    env: &mut Environment,
    delegate: id,
    take: impl FnOnce(&mut AppPickerDelegateHostObject) -> T,
) -> T {
    let mut host_obj = env.objc.borrow_mut::<AppPickerDelegateHostObject>(delegate);
    take(&mut host_obj)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncResolverAction {
    PreviousFile,
    NextFile,
    PreviousVersion,
    NextVersion,
    ChooseVersion,
    Apply,
    Cancel,
}

pub const DYLIB: crate::dyld::HostDylib = crate::dyld::HostDylib {
    // Not a real iOS dylib obviously. This shouldn't really be in the list of
    // dylibs if we can avoid it somehow (TODO?).
    path: "/.touchHLE/AppPickerHelpers.dylib",
    aliases: &[],
    class_exports: &[CLASSES],
    constant_exports: &[],
    function_exports: &[],
};

/// Be careful! These classes go in the normal class list, just like everything
/// else, so an app could try to instantiate them. Don't give them special
/// powers that could be exploited!
const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation _touchHLE_AppPickerDelegate: NSObject

- (())iconTapped:(id)sender {
    // There is no allocWithZone: that creates AppPickerDelegateHostObject, so
    // this downcast effectively acts as an assertion that this class is being
    // used within the app picker, so it can't be abused. :)
    let host_obj = env.objc.borrow_mut::<AppPickerDelegateHostObject>(this);
    host_obj.icon_tapped = sender;
}

- (())copyrightInfoShow {
    let host_obj = env.objc.borrow_mut::<AppPickerDelegateHostObject>(this);
    host_obj.copyright_show = true;
    host_obj.more_actions_dismiss = true;
}
- (())copyrightInfoHide {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).copyright_hide = true;
}
- (())copyrightInfoPrevPage {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).copyright_prev = true;
}
- (())copyrightInfoNextPage {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).copyright_next = true;
}

- (())quickOptionsShow {
    let host_obj = env.objc.borrow_mut::<AppPickerDelegateHostObject>(this);
    host_obj.quick_options_show = true;
    host_obj.more_actions_dismiss = true;
}
- (())quickOptionsHide {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).quick_options_hide = true;
}
- (())scaleHackDefault {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).scale_hack_default = true;
}
- (())scaleHack1 {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).scale_hack1 = true;
}
- (())scaleHack2 {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).scale_hack2 = true;
}
- (())scaleHack3 {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).scale_hack3 = true;
}
- (())scaleHack4 {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).scale_hack4 = true;
}
- (())orientationDefault {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).orientation_default = true;
}
- (())orientationPortraitUpsideDown {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).orientation_portrait_upside_down = true;
}
- (())orientationLandscapeLeft {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).orientation_landscape_left = true;
}
- (())orientationLandscapeRight {
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).orientation_landscape_right = true;
}
- (())analogStickTiltControls:(id)switch { // UISwitch*
    let switch_state: bool = msg![env; switch isOn];
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).analog_stick_tilt_controls = Some(switch_state);
}
- (())network:(id)switch { // UISwitch*
    let switch_state: bool = msg![env; switch isOn];
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).network = Some(switch_state);
}
- (())fullscreen:(id)switch { // UISwitch*
    let switch_state: bool = msg![env; switch isOn];
    env.objc.borrow_mut::<AppPickerDelegateHostObject>(this).fullscreen = Some(switch_state);
}

- (())syncSettingsShow {
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .sync_settings_show = true;
}
- (())syncSettingsHide {
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .sync_settings_hide = true;
}
- (())syncToggle:(id)switch {
    let enabled: bool = msg![env; switch isOn];
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .sync_toggle = Some(enabled);
}
- (())syncConnect {
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .sync_connect = true;
}
- (())moreActionsToggle {
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .more_actions_toggle = true;
}
- (())moreActionsDismiss {
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .more_actions_dismiss = true;
}

- (())openFileManager {
    // Assert (see above).
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .more_actions_dismiss = true;

    match paths::url_for_opening_user_data_dir() {
        Ok(url) => {
            // Our `openURL:` implementation is bypassed because it doesn't
            // allow non-web URLs.
            let url_res = crate::window::open_url(env, &url);
            if let Err(e) = url_res {
                echo!("Couldn't open file manager at {:?}: {}", url, e);
            } else {
                echo!("Opened file manager at {:?}.", url);
            }
        },
        Err(e) => echo!("Couldn't open file manager: {}", e),
    }
}

- (())visitWebsite {
    // Assert (see above).
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .more_actions_dismiss = true;

    let url = ns_string::get_static_str(env, "https://touchhle.org/");
    let url: id = msg_class![env; NSURL URLWithString:url];
    let ui_application: id = msg_class![env; UIApplication sharedApplication];
    assert!(msg![env; ui_application openURL:url]);
}

- (())syncAction:(id)sender {
    let tag: i32 = msg![env; sender tag];
    let action = match tag {
        0 => SyncResolverAction::PreviousFile,
        1 => SyncResolverAction::NextFile,
        2 => SyncResolverAction::PreviousVersion,
        3 => SyncResolverAction::NextVersion,
        4 => SyncResolverAction::ChooseVersion,
        5 => SyncResolverAction::Apply,
        6 => SyncResolverAction::Cancel,
        _ => panic!("invalid sync resolver action"),
    };
    env.objc
        .borrow_mut::<AppPickerDelegateHostObject>(this)
        .sync_action = Some(action);
}

@end

};

fn show_app_picker_gui<F>(
    options: Options,
    sync_root: PathBuf,
    apps: Result<Vec<AppInfo>, String>,
    live_control: Option<LiveControl>,
    run_sync: F,
) -> Result<Option<(PathBuf, Vec<String>)>, String>
where
    F: FnMut() -> Result<(Option<LiveControl>, Result<PickerSyncResult, String>), String> + 'static,
{
    let icon = picker_icon();
    let environment = Environment::new_without_app(options, icon)?;
    Ok(environment
        .run_app_picker(move |env| app_picker_inner(env, sync_root, apps, live_control, run_sync)))
}

fn app_picker_inner<F>(
    env: &mut Environment,
    sync_root: PathBuf,
    initial_apps: Result<Vec<AppInfo>, String>,
    mut live_control: Option<LiveControl>,
    mut run_sync: F,
) -> Option<(PathBuf, Vec<String>)>
where
    F: FnMut() -> Result<(Option<LiveControl>, Result<PickerSyncResult, String>), String>,
{
    let mut option_args = Vec::new();
    // Note that objects are generally not released in this code, because they
    // don't need to be: the entire Environment is thrown away at the end.

    // Bypassing UIApplicationMain!
    let ui_application: id = msg_class![env; UIApplication new];
    let delegate = env
        .objc
        .get_known_class("_touchHLE_AppPickerDelegate", &mut env.mem);
    let delegate = env.objc.alloc_object(
        delegate,
        Box::<AppPickerDelegateHostObject>::default(),
        &mut env.mem,
    );
    () = msg![env; ui_application setDelegate:delegate];

    let screen: id = msg_class![env; UIScreen mainScreen];
    let picker_auto_rotate = env.window().is_picker_auto_rotating();
    let mut picker_orientation = env.window().current_rotation();
    let picker_portrait_size = env
        .window()
        .logical_screen_size(DeviceOrientation::Portrait);
    let bounds: CGRect = if picker_auto_rotate {
        CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: picker_portrait_size.0 as CGFloat,
                height: picker_portrait_size.1 as CGFloat,
            },
        }
    } else {
        msg![env; screen bounds]
    };

    let window: id = msg_class![env; UIWindow alloc];
    let window: id = msg![env; window initWithFrame:bounds];

    let app_frame: CGRect = if picker_auto_rotate {
        let status_bar_hidden: bool = msg![env; ui_application isStatusBarHidden];
        let status_bar_height = if status_bar_hidden { 0.0 } else { 20.0 };
        CGRect {
            origin: CGPoint {
                x: 0.0,
                y: status_bar_height,
            },
            size: CGSize {
                width: bounds.size.width,
                height: bounds.size.height - status_bar_height,
            },
        }
    } else {
        msg![env; screen applicationFrame]
    };
    let main_view: id = msg_class![env; UIView alloc];
    let main_view: id = msg![env; main_view initWithFrame:app_frame];
    () = msg![env; window addSubview:main_view];

    // Wallpaper
    let mut found_wallpaper = false;
    let mut have_wallpaper = false;
    for candidate in paths::WALLPAPER_FILES {
        let candidate = paths::user_data_base_path().join(candidate);
        if !candidate.exists() {
            continue;
        }
        found_wallpaper = true;

        let image = match std::fs::read(&candidate) {
            Ok(image) => image,
            Err(e) => {
                log!("Warning: couldn't read {}: {}", candidate.display(), e);
                break;
            }
        };
        let image = match Image::from_bytes(&image) {
            Ok(image) => image,
            Err(e) => {
                log!("Warning: couldn't decode {}: {}", candidate.display(), e);
                break;
            }
        };

        let image = cg_image::from_image(env, image);
        let image: id = msg_class![env; UIImage imageWithCGImage:image];
        let wallpaper: id = msg_class![env; UIImageView alloc];
        let wallpaper: id = msg![env; wallpaper initWithImage:image];
        () = msg![env; wallpaper setFrame:(CGRect {
            origin: CGPoint {
                x: 0.0,
                y: 0.0,
            },
            size: app_frame.size,
        })];
        () = msg![env; wallpaper setAlpha:(0.5 as CGFloat)];
        () = msg![env; main_view addSubview:wallpaper];
        have_wallpaper = true;
        break;
    }
    if !found_wallpaper {
        let CGSize { width, height } = app_frame.size;
        log!(
            "No wallpaper found; filename can be one of: {}; ideal size is {}×{} pixels",
            paths::WALLPAPER_FILES.join(", "),
            width,
            height,
        );
    }

    // Version label
    let version_label: id = {
        let label_frame = CGRect {
            origin: CGPoint {
                x: 0.0,
                y: app_frame.size.height - 20.0,
            },
            size: CGSize {
                width: app_frame.size.width - 5.0,
                height: 15.0,
            },
        };
        let label: id = msg_class![env; UILabel alloc];
        let label: id = msg![env; label initWithFrame:label_frame];
        let text = ns_string::from_rust_string(
            env,
            format!(
                "touchHLE {}{}{}",
                crate::branding(),
                if crate::branding().is_empty() {
                    ""
                } else {
                    " "
                },
                crate::VERSION
            ),
        );
        () = msg![env; label setText:text];
        () = msg![env; label setTextAlignment:UITextAlignmentRight];
        let font_size: CGFloat = 12.0;
        let font: id = msg_class![env; UIFont systemFontOfSize:font_size];
        () = msg![env; label setFont:font];
        let text_color: id = if have_wallpaper {
            msg_class![env; UIColor whiteColor]
        } else {
            msg_class![env; UIColor lightGrayColor]
        };
        () = msg![env; label setTextColor:text_color];
        let bg_color: id = msg_class![env; UIColor clearColor];
        () = msg![env; label setBackgroundColor:bg_color];
        () = msg![env; main_view addSubview:label];
        label
    };

    let brand_color: id = if crate::branding() == "UNOFFICIAL" {
        msg_class![env; UIColor redColor]
    } else {
        msg_class![env; UIColor grayColor]
    };

    let mut branding_labels = Vec::with_capacity(7);
    for i in 1..=7 {
        let label_frame = CGRect {
            origin: CGPoint {
                x: 0.0,
                y: (app_frame.size.height / 8.0) * (i as f32) - 25.0,
            },
            size: CGSize {
                width: app_frame.size.width,
                height: 50.0,
            },
        };
        let label: id = msg_class![env; UILabel alloc];
        let label: id = msg![env; label initWithFrame:label_frame];
        let text = ns_string::from_rust_string(env, crate::branding().to_owned());
        () = msg![env; label setText:text];
        () = msg![env; label setTextAlignment:(if i % 2 == 0 { UITextAlignmentLeft } else { UITextAlignmentRight })];
        let font_size: CGFloat = 48.0;
        let font: id = msg_class![env; UIFont systemFontOfSize:font_size];
        () = msg![env; label setFont:font];
        () = msg![env; label setTextColor:brand_color];
        let bg_color: id = msg_class![env; UIColor clearColor];
        () = msg![env; label setBackgroundColor:bg_color];
        () = msg![env; main_view addSubview:label];
        branding_labels.push(label);
    }

    let mut app_frame = app_frame;
    let mut divider = app_frame.size.height - 100.0;

    let apps_dir = sync_root.join(paths::APPS_DIR);
    let mut apps = Vec::new();
    let mut picker_grid = PickerGridUi::default();
    refresh_picker_grid(
        env,
        delegate,
        main_view,
        app_frame,
        divider,
        have_wallpaper,
        &mut picker_grid,
        &mut apps,
        initial_apps,
    );

    let buttons_row_center = divider + (app_frame.size.height - divider) / 2.0;
    let footer_buttons = make_button_row(
        env,
        delegate,
        main_view,
        app_frame.size,
        buttons_row_center,
        &[
            ("Google Drive Sync", "syncSettingsShow"),
            ("More options", "moreActionsToggle"),
        ],
        None,
    );
    layout_picker_footer_buttons(env, &footer_buttons, app_frame.size, divider);
    let more_actions_stuff =
        setup_more_actions(env, delegate, main_view, app_frame, picker_orientation);

    let copyright_info_text = crate::licenses::get_text();
    let mut copyright_info_stuff =
        setup_copyright_info(env, delegate, main_view, app_frame, picker_orientation);
    let mut copyright_info_page_idx = 0;

    let quick_options_stuff =
        setup_quick_options(env, delegate, main_view, app_frame, picker_orientation);
    let sync_settings_stuff =
        setup_cloud_sync_settings(env, delegate, main_view, app_frame, picker_orientation);
    let settings_file = settings_path(&paths::user_data_base_path());
    let mut sync_settings = load_settings(&settings_file).unwrap_or_default();
    let mut sync_account_connected = auth::has_saved_credentials().unwrap_or(false);
    let mut sync_auth_status = String::new();
    let mut live_status = picker_live_status(&sync_root, live_control.is_some(), None);
    let mut pending_authorization: Option<PendingAuthorization> = None;
    update_cloud_sync_settings(
        env,
        &sync_settings_stuff,
        &sync_settings,
        &live_status,
        sync_account_connected,
        &sync_auth_status,
    );
    layout_cloud_sync_settings(
        env,
        &sync_settings_stuff,
        app_frame.size,
        picker_orientation,
    );
    if picker_auto_rotate && picker_orientation != DeviceOrientation::Portrait {
        app_frame = reflow_picker_views(
            env,
            screen,
            window,
            main_view,
            DeviceOrientation::Portrait,
            picker_orientation,
            picker_portrait_size,
        );
        divider = app_frame.size.height - 100.0;
        layout_picker_branding(env, version_label, &branding_labels, app_frame.size);
        layout_picker_footer_buttons(env, &footer_buttons, app_frame.size, divider);
        layout_more_actions(env, &more_actions_stuff, app_frame.size, picker_orientation);
        layout_quick_options(
            env,
            &quick_options_stuff,
            app_frame.size,
            picker_orientation,
        );
        layout_copyright_info(
            env,
            &mut copyright_info_stuff,
            app_frame.size,
            picker_orientation,
        );
        layout_cloud_sync_settings(
            env,
            &sync_settings_stuff,
            app_frame.size,
            picker_orientation,
        );
        refresh_picker_grid(
            env,
            delegate,
            main_view,
            app_frame,
            divider,
            have_wallpaper,
            &mut picker_grid,
            &mut apps,
            refresh_apps(&apps_dir),
        );
        update_cloud_sync_settings(
            env,
            &sync_settings_stuff,
            &sync_settings,
            &live_status,
            sync_account_connected,
            &sync_auth_status,
        );
        change_copyright_page(env, &mut copyright_info_stuff, &copyright_info_text, 0);
    }
    let mut quick_options_scale_hack: Option<NonZeroU32> = None;
    let mut quick_options_fullscreen: Option<()> = None;
    let mut quick_options_orientation: Option<DeviceOrientation> = None;
    let mut quick_options_analog_stick_tilt_controls = true;
    let mut quick_options_network = false;
    let mut more_actions_open = false;
    let mut sync_settings_open = false;
    let mut quick_options_open = false;
    let mut copyright_info_open = false;

    fn update_quick_option_buttons(env: &mut Environment, buttons: &[id], selected_idx: usize) {
        for (idx, &button) in buttons.iter().enumerate() {
            let color: id = if idx == selected_idx {
                msg_class![env; UIColor magentaColor]
            } else {
                msg_class![env; UIColor grayColor]
            };
            () = msg![env; button setBackgroundColor:color];
        }
    }
    fn update_scale_hack_buttons(env: &mut Environment, buttons: &[id], value: Option<NonZeroU32>) {
        update_quick_option_buttons(env, buttons, value.map_or(0, |v| v.get() as usize));
    }
    fn update_orientation_buttons(
        env: &mut Environment,
        buttons: &[id],
        value: Option<DeviceOrientation>,
    ) {
        update_quick_option_buttons(
            env,
            buttons,
            value.map_or(0, |v| match v {
                DeviceOrientation::LandscapeLeft => 1,
                DeviceOrientation::LandscapeRight => 2,
                DeviceOrientation::PortraitUpsideDown => 3,
                _ => panic!(),
            }),
        );
    }
    update_scale_hack_buttons(
        env,
        &quick_options_stuff.scale_hack_buttons,
        quick_options_scale_hack,
    );
    update_orientation_buttons(
        env,
        &quick_options_stuff.orientation_buttons,
        quick_options_orientation,
    );

    () = msg![env; window makeKeyAndVisible];

    let main_run_loop: id = msg_class![env; NSRunLoop mainRunLoop];
    // If an app is picked, this loop returns. If the user quits touchHLE, the
    // process exits.
    let mut sync_ui_changed = false;
    let mut refresh_debounce = PickerRefreshDebounce::default();
    let app_path = loop {
        let mut request_sync = false;
        run_run_loop_single_iteration(env, main_run_loop);
        if env.exit_requested() {
            return None;
        }
        let current_picker_orientation = env.window().current_rotation();
        if picker_auto_rotate && current_picker_orientation != picker_orientation {
            app_frame = reflow_picker_views(
                env,
                screen,
                window,
                main_view,
                picker_orientation,
                current_picker_orientation,
                picker_portrait_size,
            );
            picker_orientation = current_picker_orientation;
            divider = app_frame.size.height - 100.0;
            layout_picker_branding(env, version_label, &branding_labels, app_frame.size);
            layout_picker_footer_buttons(env, &footer_buttons, app_frame.size, divider);
            layout_more_actions(env, &more_actions_stuff, app_frame.size, picker_orientation);
            layout_cloud_sync_settings(
                env,
                &sync_settings_stuff,
                app_frame.size,
                picker_orientation,
            );
            layout_quick_options(
                env,
                &quick_options_stuff,
                app_frame.size,
                picker_orientation,
            );
            layout_copyright_info(
                env,
                &mut copyright_info_stuff,
                app_frame.size,
                picker_orientation,
            );
            refresh_picker_grid(
                env,
                delegate,
                main_view,
                app_frame,
                divider,
                have_wallpaper,
                &mut picker_grid,
                &mut apps,
                refresh_apps(&apps_dir),
            );
            keep_more_actions_above_picker(env, main_view, &more_actions_stuff, more_actions_open);
            keep_cloud_sync_settings_above_picker(
                env,
                main_view,
                &sync_settings_stuff,
                sync_settings_open,
            );
            keep_quick_options_above_picker(
                env,
                main_view,
                &quick_options_stuff,
                quick_options_open,
            );
            keep_copyright_info_above_picker(
                env,
                main_view,
                &copyright_info_stuff,
                copyright_info_open,
            );
            if copyright_info_open {
                copyright_info_page_idx = 0;
                change_copyright_page(
                    env,
                    &mut copyright_info_stuff,
                    &copyright_info_text,
                    copyright_info_page_idx,
                );
            }
            if sync_settings_open {
                update_cloud_sync_settings(
                    env,
                    &sync_settings_stuff,
                    &sync_settings,
                    &live_status,
                    sync_account_connected,
                    &sync_auth_status,
                );
            }
        }
        let now = Instant::now();
        while let Some(event) = live_control
            .as_ref()
            .and_then(crate::sync::live::LiveControl::try_picker_event)
        {
            refresh_debounce.record(&sync_root, &event, now);
        }
        while let Some(status) = live_control
            .as_ref()
            .and_then(crate::sync::live::LiveControl::try_status_event)
        {
            live_status = status;
            sync_ui_changed = true;
        }
        if refresh_debounce.take_due(Instant::now()) {
            refresh_picker_grid(
                env,
                delegate,
                main_view,
                app_frame,
                divider,
                have_wallpaper,
                &mut picker_grid,
                &mut apps,
                refresh_apps(&apps_dir),
            );
            keep_more_actions_above_picker(env, main_view, &more_actions_stuff, more_actions_open);
            keep_cloud_sync_settings_above_picker(
                env,
                main_view,
                &sync_settings_stuff,
                sync_settings_open,
            );
            keep_quick_options_above_picker(
                env,
                main_view,
                &quick_options_stuff,
                quick_options_open,
            );
            keep_copyright_info_above_picker(
                env,
                main_view,
                &copyright_info_stuff,
                copyright_info_open,
            );
        }
        if let Some(result) = pending_authorization
            .as_ref()
            .map(PendingAuthorization::try_result)
        {
            match result {
                Ok(Some(Ok(tokens))) => {
                    #[cfg(target_os = "android")]
                    log!("Android authorization result reached the app picker");
                    match auth::save_authorized_tokens(&tokens) {
                        Ok(()) => {
                            #[cfg(target_os = "android")]
                            log!("Android Google authorization tokens saved successfully");
                            sync_account_connected = true;
                            let updated = SyncSettings { enabled: true };
                            match save_settings(&settings_file, &updated) {
                                Ok(()) => {
                                    sync_settings = updated;
                                    sync_auth_status =
                                        "Google Drive connected. Sync enabled.".to_owned();
                                    request_sync_if_enabled(
                                        &mut request_sync,
                                        sync_settings.enabled,
                                    );
                                    if let Some(control) = &live_control {
                                        if let Err(error) =
                                            control.set_enabled(sync_settings.enabled)
                                        {
                                            sync_auth_status =
                                                crate::sync::status::redacted_error(&error);
                                        }
                                    }
                                }
                                Err(error) => {
                                    sync_auth_status = format!(
                                        "Google Drive connected, but sync could not be enabled: {}",
                                        crate::sync::status::redacted_error(&error)
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            #[cfg(target_os = "android")]
                            log!("Could not save Android Google authorization tokens: {error}");
                            sync_auth_status = redacted_auth_error(&error).to_owned();
                        }
                    }
                    pending_authorization = None;
                    sync_ui_changed = true;
                }
                Ok(Some(Err(error))) | Err(error) => {
                    sync_auth_status = redacted_auth_error(&error).to_owned();
                    pending_authorization = None;
                    sync_ui_changed = true;
                }
                Ok(None) => {}
            }
        }
        let icon_tapped = take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.icon_tapped)
        });
        if icon_tapped != nil {
            let tapped = picker_grid
                .icon_grid
                .as_ref()
                .and_then(|grid| grid.icon_map.get(&icon_tapped))
                .copied();
            match tapped {
                Some(TappedIcon::App(app_idx)) => {
                    let Some(app_path) = apps.get(app_idx).map(|app| app.path.clone()) else {
                        continue;
                    };
                    // Provide visual feedback that the app has been picked
                    // (it may take a while for the splash screen to appear etc)
                    () = msg![env; icon_tapped setAlpha:(0.5 as CGFloat)];
                    // Redraw screen, even if this makes the next frame early
                    // (the app picker will never be redrawn after this).
                    crate::frameworks::core_animation::recomposite_if_necessary(
                        env, /* force: */ true,
                    );
                    // Ensure touchHLE is responsive from the OS perspective,
                    // otherwise screen redraw might not show up? (Unclear if
                    // this explanation is correct.)
                    run_run_loop_single_iteration(env, main_run_loop);
                    if env.exit_requested() {
                        return None;
                    }

                    echo!("Picked: {}", app_path.display());
                    break app_path;
                }
                Some(TappedIcon::ChangePage(page_idx)) => {
                    if let Some(grid) = picker_grid.icon_grid.as_mut() {
                        if page_idx < grid.pages.len() {
                            picker_grid.model.current_page = page_idx;
                            update_icon_grid(env, grid, &mut apps, page_idx);
                        }
                    }
                }
                None => (), // Tapped on a black space
            }
            continue;
        }
        if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.more_actions_toggle)
        }) {
            more_actions_open = !more_actions_open;
            () = msg![env; (more_actions_stuff.main_view) setHidden:(!more_actions_open)];
            keep_more_actions_above_picker(env, main_view, &more_actions_stuff, more_actions_open);
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.more_actions_dismiss)
        }) {
            more_actions_open = false;
            () = msg![env; (more_actions_stuff.main_view) setHidden:true];
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.sync_settings_show)
        }) {
            live_status =
                picker_live_status(&sync_root, live_control.is_some(), Some(&live_status));
            more_actions_open = false;
            () = msg![env; (more_actions_stuff.main_view) setHidden:true];
            () = msg![env; (quick_options_stuff.main_view) setHidden:true];
            () = msg![env; (copyright_info_stuff.main_view) setHidden:true];
            () = msg![env; (sync_settings_stuff.main_view) setHidden:false];
            layout_cloud_sync_settings(
                env,
                &sync_settings_stuff,
                app_frame.size,
                picker_orientation,
            );
            sync_settings_open = true;
            keep_cloud_sync_settings_above_picker(
                env,
                main_view,
                &sync_settings_stuff,
                sync_settings_open,
            );
            sync_ui_changed = true;
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.sync_settings_hide)
        }) {
            () = msg![env; (sync_settings_stuff.main_view) setHidden:true];
            sync_settings_open = false;
        } else if let Some(enabled) = take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.sync_toggle)
        }) {
            let updated = SyncSettings { enabled };
            match save_settings(&settings_file, &updated) {
                Ok(()) => {
                    sync_settings = updated;
                    sync_auth_status.clear();
                    request_sync_if_enabled(&mut request_sync, enabled);
                    if let Some(control) = &live_control {
                        if let Err(error) = control.set_enabled(sync_settings.enabled) {
                            sync_auth_status = crate::sync::status::redacted_error(&error);
                        }
                    }
                }
                Err(error) => sync_auth_status = crate::sync::status::redacted_error(&error),
            }
            sync_ui_changed = true;
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.sync_connect)
        }) {
            match (
                auth::authorization_available(),
                pending_authorization.is_none(),
            ) {
                (true, true) => {
                    let client_id = auth::configured_client_id().unwrap_or_default();
                    match auth::start_authorization(client_id.to_owned(), |url| {
                        crate::window::open_url(env, url).map_err(|error| error.to_string())
                    }) {
                        Ok(pending) => {
                            pending_authorization = Some(pending);
                            sync_auth_status = if cfg!(target_os = "android") {
                                "Waiting for Google authorization...".to_owned()
                            } else {
                                "Waiting for Google authorization in the browser...".to_owned()
                            };
                        }
                        Err(error) => sync_auth_status = redacted_auth_error(&error).to_owned(),
                    }
                }
                (true, false) => {
                    sync_auth_status = "Google authorization is already in progress.".to_owned();
                }
                (false, _) => {
                    sync_auth_status =
                        "Google OAuth client ID is not configured for this build.".to_owned();
                }
            }
            sync_ui_changed = true;
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.copyright_show)
        }) {
            copyright_info_page_idx = 0;
            layout_copyright_info(
                env,
                &mut copyright_info_stuff,
                app_frame.size,
                picker_orientation,
            );
            change_copyright_page(
                env,
                &mut copyright_info_stuff,
                &copyright_info_text,
                copyright_info_page_idx,
            );
            () = msg![env; (copyright_info_stuff.main_view) setHidden:false];
            copyright_info_open = true;
            keep_copyright_info_above_picker(
                env,
                main_view,
                &copyright_info_stuff,
                copyright_info_open,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.copyright_hide)
        }) {
            () = msg![env; (copyright_info_stuff.main_view) setHidden:true];
            copyright_info_open = false;
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.copyright_prev)
        }) && copyright_info_page_idx != 0
        {
            copyright_info_page_idx -= 1;
            change_copyright_page(
                env,
                &mut copyright_info_stuff,
                &copyright_info_text,
                copyright_info_page_idx,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.copyright_next)
        }) && Some(copyright_info_page_idx) != copyright_info_stuff.last_page_idx
        {
            copyright_info_page_idx += 1;
            change_copyright_page(
                env,
                &mut copyright_info_stuff,
                &copyright_info_text,
                copyright_info_page_idx,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.quick_options_show)
        }) {
            () = msg![env; (quick_options_stuff.main_view) setHidden:false];
            quick_options_open = true;
            layout_quick_options(
                env,
                &quick_options_stuff,
                app_frame.size,
                picker_orientation,
            );
            keep_quick_options_above_picker(
                env,
                main_view,
                &quick_options_stuff,
                quick_options_open,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.quick_options_hide)
        }) {
            () = msg![env; (quick_options_stuff.main_view) setHidden:true];
            quick_options_open = false;
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.scale_hack_default)
        }) {
            quick_options_scale_hack = None;
            update_scale_hack_buttons(
                env,
                &quick_options_stuff.scale_hack_buttons,
                quick_options_scale_hack,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.scale_hack1)
        }) {
            quick_options_scale_hack = Some(NonZeroU32::new(1).unwrap());
            update_scale_hack_buttons(
                env,
                &quick_options_stuff.scale_hack_buttons,
                quick_options_scale_hack,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.scale_hack2)
        }) {
            quick_options_scale_hack = Some(NonZeroU32::new(2).unwrap());
            update_scale_hack_buttons(
                env,
                &quick_options_stuff.scale_hack_buttons,
                quick_options_scale_hack,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.scale_hack3)
        }) {
            quick_options_scale_hack = Some(NonZeroU32::new(3).unwrap());
            update_scale_hack_buttons(
                env,
                &quick_options_stuff.scale_hack_buttons,
                quick_options_scale_hack,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.scale_hack4)
        }) {
            quick_options_scale_hack = Some(NonZeroU32::new(4).unwrap());
            update_scale_hack_buttons(
                env,
                &quick_options_stuff.scale_hack_buttons,
                quick_options_scale_hack,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.orientation_default)
        }) {
            quick_options_orientation = None;
            update_orientation_buttons(
                env,
                &quick_options_stuff.orientation_buttons,
                quick_options_orientation,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.orientation_portrait_upside_down)
        }) {
            quick_options_orientation = Some(DeviceOrientation::PortraitUpsideDown);
            update_orientation_buttons(
                env,
                &quick_options_stuff.orientation_buttons,
                quick_options_orientation,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.orientation_landscape_left)
        }) {
            quick_options_orientation = Some(DeviceOrientation::LandscapeLeft);
            update_orientation_buttons(
                env,
                &quick_options_stuff.orientation_buttons,
                quick_options_orientation,
            );
        } else if take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.orientation_landscape_right)
        }) {
            quick_options_orientation = Some(DeviceOrientation::LandscapeRight);
            update_orientation_buttons(
                env,
                &quick_options_stuff.orientation_buttons,
                quick_options_orientation,
            );
        } else if let Some(enabled) = take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.analog_stick_tilt_controls)
        }) {
            quick_options_analog_stick_tilt_controls = enabled;
        } else if let Some(enabled) = take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.network)
        }) {
            quick_options_network = enabled;
        } else if let Some(fullscreen) = take_picker_host_value(env, delegate, |host_obj| {
            std::mem::take(&mut host_obj.fullscreen)
        }) {
            quick_options_fullscreen = match fullscreen {
                false => None,
                true => Some(()),
            };
        }
        if request_sync {
            sync_auth_status = "Syncing with Google Drive...".to_owned();
            update_cloud_sync_settings(
                env,
                &sync_settings_stuff,
                &sync_settings,
                &live_status,
                sync_account_connected,
                &sync_auth_status,
            );
            match run_sync() {
                Ok((new_live_control, result)) => {
                    live_control = new_live_control;
                    sync_auth_status = match result {
                        Ok(PickerSyncResult::Completed) => {
                            "Google Drive sync completed.".to_owned()
                        }
                        Ok(PickerSyncResult::Offline) => {
                            "Cloud sync unavailable; continuing with local files.".to_owned()
                        }
                        Err(error) => format!("Google Drive sync failed: {error}"),
                    };
                }
                Err(error) => {
                    live_control = None;
                    sync_auth_status = format!("Google Drive sync failed: {error}");
                }
            }
            live_status =
                picker_live_status(&sync_root, live_control.is_some(), Some(&live_status));
            refresh_picker_grid(
                env,
                delegate,
                main_view,
                app_frame,
                divider,
                have_wallpaper,
                &mut picker_grid,
                &mut apps,
                refresh_apps(&apps_dir),
            );
            keep_more_actions_above_picker(env, main_view, &more_actions_stuff, more_actions_open);
            keep_cloud_sync_settings_above_picker(
                env,
                main_view,
                &sync_settings_stuff,
                sync_settings_open,
            );
            keep_quick_options_above_picker(
                env,
                main_view,
                &quick_options_stuff,
                quick_options_open,
            );
            keep_copyright_info_above_picker(
                env,
                main_view,
                &copyright_info_stuff,
                copyright_info_open,
            );
            sync_ui_changed = true;
        }
        if sync_ui_changed {
            update_cloud_sync_settings(
                env,
                &sync_settings_stuff,
                &sync_settings,
                &live_status,
                sync_account_connected,
                &sync_auth_status,
            );
            sync_ui_changed = false;
        }
    };

    // Apply user-specified overrides
    if let Some(scale_hack) = quick_options_scale_hack {
        option_args.push(format!("--scale-hack={}", scale_hack.get()));
    }
    if let Some(orientation) = quick_options_orientation {
        option_args.push(
            match orientation {
                DeviceOrientation::LandscapeLeft => "--landscape-left",
                DeviceOrientation::LandscapeRight => "--landscape-right",
                DeviceOrientation::PortraitUpsideDown => "--upside-down",
                _ => todo!(),
            }
            .to_string(),
        );
    }
    if let Some(()) = quick_options_fullscreen {
        option_args.push("--fullscreen".to_string());
    }
    if !quick_options_analog_stick_tilt_controls {
        option_args.push("--disable-analog-stick-tilt-controls".to_string());
    }
    if quick_options_network {
        option_args.push("--allow-network-access".to_string());
    }

    // Return the environment so some parts of it can be salvaged.
    Some((app_path, option_args))
}

const ICON_SIZE: CGSize = CGSize {
    width: 57.0,
    height: 57.0,
};

#[derive(Clone, Copy)]
enum TappedIcon {
    App(usize),
    ChangePage(usize),
}

#[derive(Default)]
struct PickerGridModel {
    app_count: usize,
    current_page: usize,
}

impl PickerGridModel {
    fn replace_apps(&mut self, app_count: usize, slots_per_page: usize) {
        self.app_count = app_count;
        self.current_page = clamp_page_index(
            self.current_page,
            app_pages(app_count, slots_per_page).len(),
        );
    }
}

#[derive(Default)]
struct PickerGridUi {
    icon_grid: Option<IconGridStuff>,
    empty_state_label: Option<id>,
    error_label: Option<id>,
    model: PickerGridModel,
}

struct IconGridStuff {
    icon_buttons_and_labels: Vec<(id, id)>,
    placeholder_icon: Option<id>,
    prev_icon: Option<id>,
    next_icon: Option<id>,
    pages: Vec<std::ops::Range<usize>>,
    icon_map: HashMap<id, TappedIcon>,
}

#[derive(Default)]
struct PickerRefreshDebounce {
    due_at: Option<Instant>,
}

impl PickerRefreshDebounce {
    fn record(&mut self, sync_root: &Path, event: &LiveEvent, now: Instant) {
        if affects_apps(sync_root, event) {
            self.due_at = Some(now + Duration::from_secs(1));
        }
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if self.due_at.is_some_and(|due_at| now >= due_at) {
            self.due_at = None;
            true
        } else {
            false
        }
    }
}

fn request_sync_if_enabled(requested: &mut bool, enabled: bool) {
    *requested |= enabled;
}

fn clamp_page_index(page_index: usize, page_count: usize) -> usize {
    page_index.min(page_count.saturating_sub(1))
}

fn picker_grid_dimensions(app_frame: CGRect) -> (usize, usize) {
    if app_frame.size.width > app_frame.size.height {
        (5, 2)
    } else {
        (4, 4)
    }
}

fn app_pages(total_app_count: usize, slots_per_page: usize) -> Vec<std::ops::Range<usize>> {
    if slots_per_page <= 2 {
        return (0..total_app_count).map(|index| index..index + 1).collect();
    }
    let mut pages = Vec::new();
    let mut start = 0;
    while start < total_app_count && slots_per_page > 1 {
        let mut end = start + slots_per_page;
        if start > 0 {
            end -= 1;
        }
        if end < total_app_count {
            end -= 1;
        } else {
            end = total_app_count;
        }
        pages.push(start..end);
        start = end;
    }
    pages
}

fn refresh_picker_grid(
    env: &mut Environment,
    delegate: id,
    main_view: id,
    app_frame: CGRect,
    divider: CGFloat,
    have_wallpaper: bool,
    picker_grid: &mut PickerGridUi,
    apps: &mut Vec<AppInfo>,
    refreshed_apps: Result<Vec<AppInfo>, String>,
) {
    if let Some(mut grid) = picker_grid.icon_grid.take() {
        grid.icon_map.clear();
        for (icon_button, label) in grid.icon_buttons_and_labels {
            () = msg![env; icon_button removeFromSuperview];
            remove_picker_label(env, label);
        }
    }
    if let Some(label) = picker_grid.empty_state_label.take() {
        remove_picker_label(env, label);
    }
    if let Some(label) = picker_grid.error_label.take() {
        remove_picker_label(env, label);
    }
    apps.clear();

    let message_frame = CGRect {
        origin: CGPoint { x: 10.0, y: 10.0 },
        size: CGSize {
            width: app_frame.size.width - 20.0,
            height: divider - 20.0,
        },
    };
    match refreshed_apps {
        Ok(refreshed_apps) if refreshed_apps.is_empty() => {
            let (columns, rows) = picker_grid_dimensions(app_frame);
            picker_grid.model.replace_apps(0, columns * rows);
            let label: id = msg_class![env; UILabel alloc];
            let label: id = msg![env; label initWithFrame:message_frame];
            let text = ns_string::get_static_str(
                env,
                "No apps found. Use File manager to add an app; the list will refresh automatically.",
            );
            () = msg![env; label setText:text];
            () = msg![env; label setTextAlignment:UITextAlignmentCenter];
            () = msg![env; label setNumberOfLines:0];
            let text_color: id = msg_class![env; UIColor lightGrayColor];
            () = msg![env; label setTextColor:text_color];
            let bg_color: id = msg_class![env; UIColor clearColor];
            () = msg![env; label setBackgroundColor:bg_color];
            () = msg![env; main_view addSubview:label];
            picker_grid.empty_state_label = Some(label);
        }
        Ok(refreshed_apps) => {
            *apps = refreshed_apps;
            let (columns, rows) = picker_grid_dimensions(app_frame);
            picker_grid.model.replace_apps(apps.len(), columns * rows);
            let mut grid = make_icon_grid(
                env,
                delegate,
                main_view,
                app_frame,
                apps.len(),
                have_wallpaper,
            );
            update_icon_grid(env, &mut grid, apps, picker_grid.model.current_page);
            picker_grid.icon_grid = Some(grid);
        }
        Err(error) => {
            let (columns, rows) = picker_grid_dimensions(app_frame);
            picker_grid.model.replace_apps(0, columns * rows);
            let label: id = msg_class![env; UILabel alloc];
            let label: id = msg![env; label initWithFrame:message_frame];
            let text = ns_string::from_rust_string(env, error);
            () = msg![env; label setText:text];
            () = msg![env; label setTextAlignment:UITextAlignmentCenter];
            () = msg![env; label setNumberOfLines:0];
            let text_color: id = msg_class![env; UIColor lightGrayColor];
            () = msg![env; label setTextColor:text_color];
            let bg_color: id = msg_class![env; UIColor clearColor];
            () = msg![env; label setBackgroundColor:bg_color];
            () = msg![env; main_view addSubview:label];
            picker_grid.error_label = Some(label);
        }
    }
}

fn remove_picker_label(env: &mut Environment, label: id) {
    () = msg![env; label removeFromSuperview];
    release(env, label);
}

fn make_icon_grid(
    env: &mut Environment,
    delegate: id,
    main_view: id,
    app_frame: CGRect,
    total_app_count: usize,
    have_wallpaper: bool,
) -> IconGridStuff {
    let (num_cols, num_rows) = picker_grid_dimensions(app_frame);
    let num_cols_f = num_cols as CGFloat;
    let label_size = CGSize {
        width: 74.0,
        height: 13.0,
    };
    let icon_gap_x: CGFloat = 19.0;
    let icon_gap_y: CGFloat = 4.0 + label_size.height + 14.0;
    let icon_grid_width = (ICON_SIZE.width * num_cols_f) + icon_gap_x * (num_cols_f - 1.0);
    let icon_grid_origin = CGPoint {
        x: (app_frame.size.width - icon_grid_width) / 2.0,
        y: 12.0,
    };

    let icon_tapped_sel = env.objc.lookup_selector("iconTapped:").unwrap();

    let mut icon_buttons_and_labels = Vec::new();

    for i in 0..(num_cols * num_rows) {
        let col = i % num_cols;
        let row = i / num_cols;

        // Rounding is needed here to avoid a blurry or offset image.
        let icon_frame = CGRect {
            origin: CGPoint {
                x: (icon_grid_origin.x + (col as CGFloat) * (ICON_SIZE.width + icon_gap_x)).round(),
                y: (icon_grid_origin.y + (row as CGFloat) * (ICON_SIZE.height + icon_gap_y))
                    .round(),
            },
            size: ICON_SIZE,
        };
        let icon_button: id = msg_class![env; UIButton buttonWithType:UIButtonTypeCustom];
        () = msg![env; icon_button setFrame:icon_frame];
        let image_view: id = msg![env; icon_button imageView];
        let bounds: CGRect = msg![env; icon_button bounds];
        () = msg![env; image_view setFrame:bounds];
        () = msg![env; icon_button addTarget:delegate
                                      action:icon_tapped_sel
                            forControlEvents:UIControlEventTouchUpInside];
        () = msg![env; main_view addSubview:icon_button];

        // Rounding is needed here to avoid blurry text.
        let label_frame = CGRect {
            origin: CGPoint {
                x: (icon_frame.origin.x - (label_size.width - ICON_SIZE.width) / 2.0).round(),
                y: (icon_frame.origin.y + ICON_SIZE.height + 4.0).round(),
            },
            size: label_size,
        };
        let label: id = msg_class![env; UILabel alloc];
        let label: id = msg![env; label initWithFrame:label_frame];
        () = msg![env; label setTextAlignment:UITextAlignmentCenter];
        let font_size: CGFloat = label_size.height - 2.0;
        let font: id = if have_wallpaper {
            msg_class![env; UIFont systemFontOfSize:font_size]
        } else {
            msg_class![env; UIFont boldSystemFontOfSize:font_size]
        };
        () = msg![env; label setFont:font];
        let text_color: id = if have_wallpaper {
            msg_class![env; UIColor whiteColor]
        } else {
            msg_class![env; UIColor lightGrayColor]
        };
        () = msg![env; label setTextColor:text_color];
        let bg_color: id = msg_class![env; UIColor clearColor];
        () = msg![env; label setBackgroundColor:bg_color];
        () = msg![env; main_view addSubview:label];

        icon_buttons_and_labels.push((icon_button, label));
    }

    // TODO: Use UIScrollView pagination and UIPageControl once available.
    let pages = app_pages(total_app_count, icon_buttons_and_labels.len());

    IconGridStuff {
        icon_buttons_and_labels,
        placeholder_icon: None,
        prev_icon: None,
        next_icon: None,
        pages,
        icon_map: HashMap::new(),
    }
}

fn make_icon_from_glyph(
    env: &mut Environment,
    glyph: char,
    font_size: CGFloat,
    baseline_offset: CGFloat,
    bg_color: (CGFloat, CGFloat, CGFloat, CGFloat),
) -> id {
    let color_space = CGColorSpaceCreateDeviceRGB(env);
    let context = CGBitmapContextCreate(
        env,
        Ptr::null(),
        ICON_SIZE.width as u32,
        ICON_SIZE.height as u32,
        8,
        4 * (ICON_SIZE.width as u32),
        color_space,
        kCGImageAlphaPremultipliedLast,
    );
    UIGraphicsPushContext(env, context);

    // Compensate for row order inversion
    CGContextTranslateCTM(env, context, 0.0, ICON_SIZE.height);
    CGContextScaleCTM(env, context, 1.0, -1.0);

    let (r, g, b, a) = bg_color;
    CGContextSetRGBFillColor(env, context, r, g, b, a);
    CGContextFillRect(
        env,
        context,
        CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: ICON_SIZE,
        },
    );

    let font: id = msg_class![env; UIFont systemFontOfSize:font_size];
    let glyph_string: id = ns_string::from_rust_string(env, [glyph].into_iter().collect());
    let glyph_size: CGSize = msg![env; glyph_string sizeWithFont:font];
    CGContextSetRGBFillColor(env, context, 1.0, 1.0, 1.0, 1.0); // white
    let glyph_origin = CGPoint {
        x: ICON_SIZE.width / 2.0 - glyph_size.width / 2.0,
        y: ICON_SIZE.height / 2.0 - glyph_size.height / 2.0 + baseline_offset,
    };
    let _: CGSize = msg![env; glyph_string drawAtPoint:glyph_origin withFont:font];
    release(env, glyph_string);

    UIGraphicsPopContext(env);

    let cg_image = CGBitmapContextCreateImage(env, context);
    // This radius should match the one in src/bundle.rs.
    cg_image::borrow_image_mut(&mut env.objc, cg_image).round_corners(
        (10.0 / 57.0) * ICON_SIZE.width,
        /* four_corners: */ true,
        /* add_sheen: */ true,
    );
    CGContextRelease(env, context);

    let ui_image: id = msg_class![env; UIImage imageWithCGImage:cg_image];
    release(env, cg_image);

    ui_image
}

fn update_icon_grid(
    env: &mut Environment,
    icon_grid_stuff: &mut IconGridStuff,
    apps: &mut [AppInfo],
    page_idx: usize,
) {
    icon_grid_stuff.icon_map.clear();

    let Some(app_idx_range) = icon_grid_stuff.pages.get(page_idx).cloned() else {
        return;
    };
    let have_prev_icon = page_idx != 0;
    let have_next_icon = app_idx_range.end != apps.len();

    let mut icon_iter = icon_grid_stuff.icon_buttons_and_labels.iter();

    if have_prev_icon {
        let Some(&(icon_button, label)) = icon_iter.next() else {
            return;
        };
        let image = *icon_grid_stuff.prev_icon.get_or_insert_with(|| {
            make_icon_from_glyph(env, '←', 50.0, -9.0, (0.25, 0.25, 0.25, 1.0))
        });
        () = msg![env; icon_button setImage:image forState:UIControlStateNormal];
        () = msg![env; label setText:(ns_string::get_static_str(env, ""))];
        icon_grid_stuff
            .icon_map
            .insert(icon_button, TappedIcon::ChangePage(page_idx - 1));
    }

    for app_idx in app_idx_range.clone() {
        let Some(app) = apps.get_mut(app_idx) else {
            return;
        };
        let Some(&(icon_button, label)) = icon_iter.next() else {
            return;
        };

        if let Some(icon) = app.icon.take() {
            let image = cg_image::from_image(env, icon);
            let image: id = msg_class![env; UIImage imageWithCGImage:image];
            app.icon_ui_image = Some(image);
        }

        let image = app.icon_ui_image.unwrap_or_else(|| {
            *icon_grid_stuff.placeholder_icon.get_or_insert_with(|| {
                make_icon_from_glyph(env, '?', 40.0, 0.0, (0.5, 0.5, 0.5, 1.0))
            })
        });
        () = msg![env; icon_button setImage:image forState:UIControlStateNormal];

        let text = *app
            .display_name_ns_string
            .get_or_insert_with(|| ns_string::from_rust_string(env, app.display_name.clone()));
        () = msg![env; label setText:text];

        icon_grid_stuff
            .icon_map
            .insert(icon_button, TappedIcon::App(app_idx));
    }

    if have_next_icon {
        let Some(&(icon_button, label)) = icon_iter.next() else {
            return;
        };
        let image = *icon_grid_stuff.next_icon.get_or_insert_with(|| {
            make_icon_from_glyph(env, '→', 50.0, -9.0, (0.25, 0.25, 0.25, 1.0))
        });
        () = msg![env; icon_button setImage:image forState:UIControlStateNormal];
        () = msg![env; label setText:(ns_string::get_static_str(env, ""))];
        icon_grid_stuff
            .icon_map
            .insert(icon_button, TappedIcon::ChangePage(page_idx + 1));
    }

    // There may be remaining spaces might need to be blanked.
    for &(icon_button, label) in icon_iter {
        () = msg![env; icon_button setImage:nil forState:UIControlStateNormal];
        () = msg![env; label setText:(ns_string::get_static_str(env, ""))];
    }
}

fn make_button_row(
    env: &mut Environment,
    delegate: id,
    super_view: id,
    super_view_size: CGSize,
    buttons_row_center: CGFloat,
    buttons: &[(&'static str, &'static str)],
    font_size: Option<CGFloat>,
) -> Vec<id> {
    let margin = 10.0;

    let button_size = CGSize {
        width: (super_view_size.width - margin) / (buttons.len() as CGFloat) - margin,
        height: 30.0,
    };
    let mut button_frame = CGRect {
        origin: CGPoint {
            x: margin,
            y: buttons_row_center - button_size.height / 2.0,
        },
        size: button_size,
    };

    let mut ui_buttons = Vec::new();
    for (title_text, selector) in buttons {
        let button: id = msg_class![env; UIButton buttonWithType:UIButtonTypeRoundedRect];
        let text = ns_string::get_static_str(env, title_text);
        () = msg![env; button setTitle:text forState:UIControlStateNormal];
        () = msg![env; button setFrame:button_frame];
        // FIXME: manually calling layoutSubviews shouldn't be needed?
        () = msg![env; button layoutSubviews];

        if let Some(font_size) = font_size {
            let label: id = msg![env; button titleLabel];
            let font: id = msg_class![env; UIFont systemFontOfSize:font_size];
            () = msg![env; label setFont:font];
        }

        let selector = env.objc.lookup_selector(selector).unwrap();
        () = msg![env; button addTarget:delegate
                                 action:selector
                       forControlEvents:UIControlEventTouchUpInside];
        () = msg![env; super_view addSubview:button];

        button_frame.origin.x += button_size.width + margin;
        ui_buttons.push(button);
    }
    ui_buttons
}

fn footer_button_frames(app_frame_size: CGSize, divider: CGFloat, count: usize) -> Vec<CGRect> {
    if count == 0 {
        return Vec::new();
    }
    let margin = 10.0;
    let footer_height = (app_frame_size.height - divider).max(0.0);
    let height = 44.0_f32.min(footer_height);
    let width =
        ((app_frame_size.width - margin * (count as CGFloat + 1.0)) / count as CGFloat).max(0.0);
    let y = divider + (footer_height - height) / 2.0;
    (0..count)
        .map(|index| CGRect {
            origin: CGPoint {
                x: margin + index as CGFloat * (width + margin),
                y,
            },
            size: CGSize { width, height },
        })
        .collect()
}

fn layout_picker_footer_buttons(
    env: &mut Environment,
    buttons: &[id],
    app_frame_size: CGSize,
    divider: CGFloat,
) {
    for (&button, frame) in
        buttons
            .iter()
            .zip(footer_button_frames(app_frame_size, divider, buttons.len()))
    {
        () = msg![env; button setFrame:frame];
        () = msg![env; button layoutSubviews];
    }
}

fn layout_picker_branding(
    env: &mut Environment,
    version_label: id,
    branding_labels: &[id],
    app_frame_size: CGSize,
) {
    let version_frame = CGRect {
        origin: CGPoint {
            x: 0.0,
            y: app_frame_size.height - 20.0,
        },
        size: CGSize {
            width: (app_frame_size.width - 5.0).max(0.0),
            height: 15.0,
        },
    };
    () = msg![env; version_label setFrame:version_frame];

    for (index, &label) in branding_labels.iter().enumerate() {
        let i = index + 1;
        let frame = CGRect {
            origin: CGPoint {
                x: 0.0,
                y: (app_frame_size.height / 8.0) * i as CGFloat - 25.0,
            },
            size: CGSize {
                width: app_frame_size.width,
                height: 50.0,
            },
        };
        () = msg![env; label setFrame:frame];
    }
}

const MORE_ACTIONS_TITLE_HEIGHT: CGFloat = 30.0;
const MORE_ACTIONS_VERTICAL_PADDING: CGFloat = 8.0;
const MORE_ACTIONS_ROW_HEIGHT: CGFloat = 44.0;
const MORE_ACTIONS_LANDSCAPE_WIDTH: CGFloat = 280.0;
const MORE_ACTIONS_PORTRAIT_WIDTH: CGFloat = 360.0;
const MORE_ACTIONS_PANEL_HEIGHT: CGFloat = MORE_ACTIONS_VERTICAL_PADDING * 2.0
    + MORE_ACTIONS_TITLE_HEIGHT
    + 8.0
    + MORE_ACTIONS_ROW_HEIGHT * 4.0;

struct MoreActionsStuff {
    main_view: id,
    panel_view: id,
    title: id,
    close_button: id,
    buttons: Vec<id>,
}

fn more_actions_panel_frame(app_frame_size: CGSize, orientation: DeviceOrientation) -> CGRect {
    let is_landscape = matches!(
        orientation,
        DeviceOrientation::LandscapeLeft | DeviceOrientation::LandscapeRight
    );
    let width = if is_landscape {
        MORE_ACTIONS_LANDSCAPE_WIDTH.min((app_frame_size.width - 16.0).max(0.0))
    } else {
        MORE_ACTIONS_PORTRAIT_WIDTH.min((app_frame_size.width - 16.0).max(0.0))
    };
    let available_height = (app_frame_size.height - 16.0).max(0.0);
    let height = MORE_ACTIONS_PANEL_HEIGHT.min(available_height);

    CGRect {
        origin: CGPoint {
            x: (app_frame_size.width - width) / 2.0,
            y: (app_frame_size.height - height) / 2.0,
        },
        size: CGSize { width, height },
    }
}

fn layout_more_actions(
    env: &mut Environment,
    ui: &MoreActionsStuff,
    app_frame_size: CGSize,
    orientation: DeviceOrientation,
) {
    let overlay_frame = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: app_frame_size,
    };
    () = msg![env; (ui.main_view) setFrame:overlay_frame];
    let panel_frame = more_actions_panel_frame(app_frame_size, orientation);
    () = msg![env; (ui.panel_view) setFrame:panel_frame];

    let title_frame = CGRect {
        origin: CGPoint {
            x: 12.0,
            y: MORE_ACTIONS_VERTICAL_PADDING,
        },
        size: CGSize {
            width: (panel_frame.size.width - 84.0).max(0.0),
            height: MORE_ACTIONS_TITLE_HEIGHT,
        },
    };
    () = msg![env; (ui.title) setFrame:title_frame];
    let close_frame = CGRect {
        origin: CGPoint {
            x: (panel_frame.size.width - 68.0).max(8.0),
            y: MORE_ACTIONS_VERTICAL_PADDING,
        },
        size: CGSize {
            width: (panel_frame.size.width - 76.0).min(56.0).max(0.0),
            height: MORE_ACTIONS_TITLE_HEIGHT,
        },
    };
    () = msg![env; (ui.close_button) setFrame:close_frame];

    let row_height = ((panel_frame.size.height
        - MORE_ACTIONS_VERTICAL_PADDING * 2.0
        - MORE_ACTIONS_TITLE_HEIGHT
        - 8.0)
        / 4.0)
        .max(0.0);
    let row_top = MORE_ACTIONS_VERTICAL_PADDING + MORE_ACTIONS_TITLE_HEIGHT + 8.0;
    for (index, &button) in ui.buttons.iter().enumerate() {
        let frame = CGRect {
            origin: CGPoint {
                x: 8.0,
                y: row_top + row_height * index as CGFloat,
            },
            size: CGSize {
                width: (panel_frame.size.width - 16.0).max(0.0),
                height: row_height,
            },
        };
        () = msg![env; button setFrame:frame];
        () = msg![env; button layoutSubviews];
    }
    () = msg![env; (ui.close_button) layoutSubviews];
}

fn setup_more_actions(
    env: &mut Environment,
    delegate: id,
    super_view: id,
    app_frame: CGRect,
    orientation: DeviceOrientation,
) -> MoreActionsStuff {
    let main_view: id = msg_class![env; UIView alloc];
    let main_view: id = msg![env; main_view initWithFrame:(CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: app_frame.size,
    })];
    let dim_background: id = msg_class![env; UIColor blackColor];
    let dim_background: id = msg![env; dim_background colorWithAlphaComponent:(0.58 as CGFloat)];
    () = msg![env; main_view setBackgroundColor:dim_background];
    () = msg![env; main_view setHidden:true];
    () = msg![env; super_view addSubview:main_view];

    let panel_view: id = msg_class![env; UIView alloc];
    let panel_view: id = msg![env; panel_view initWithFrame:(more_actions_panel_frame(
        app_frame.size,
        orientation,
    ))];
    let panel_background: id = msg_class![env; UIColor whiteColor];
    () = msg![env; panel_view setBackgroundColor:panel_background];
    () = msg![env; main_view addSubview:panel_view];

    let title: id = msg_class![env; UILabel alloc];
    let title: id = msg![env; title initWithFrame:(CGRect {
        origin: CGPoint { x: 12.0, y: MORE_ACTIONS_VERTICAL_PADDING },
        size: CGSize {
            width: (app_frame.size.width - 96.0).max(0.0),
            height: MORE_ACTIONS_TITLE_HEIGHT,
        },
    })];
    let title_text = ns_string::get_static_str(env, "More options");
    () = msg![env; title setText:title_text];
    let title_font: id = msg_class![env; UIFont boldSystemFontOfSize:(16.0 as CGFloat)];
    () = msg![env; title setFont:title_font];
    () = msg![env; panel_view addSubview:title];

    let close_button: id = msg_class![env; UIButton buttonWithType:UIButtonTypeRoundedRect];
    let close_title = ns_string::get_static_str(env, "Close");
    () = msg![env; close_button setTitle:close_title forState:UIControlStateNormal];
    let close_selector = env.objc.lookup_selector("moreActionsDismiss").unwrap();
    () = msg![env; close_button addTarget:delegate
                                   action:close_selector
                         forControlEvents:UIControlEventTouchUpInside];
    () = msg![env; panel_view addSubview:close_button];

    let actions = [
        ("File manager", "openFileManager"),
        ("Quick options", "quickOptionsShow"),
        ("Copyright info", "copyrightInfoShow"),
        ("touchHLE.org", "visitWebsite"),
    ];
    let mut buttons = Vec::with_capacity(actions.len());
    for (title_text, selector_name) in actions {
        let button: id = msg_class![env; UIButton buttonWithType:UIButtonTypeRoundedRect];
        let text = ns_string::get_static_str(env, title_text);
        () = msg![env; button setTitle:text forState:UIControlStateNormal];
        let selector = env.objc.lookup_selector(selector_name).unwrap();
        () = msg![env; button addTarget:delegate
                                 action:selector
                       forControlEvents:UIControlEventTouchUpInside];
        () = msg![env; panel_view addSubview:button];
        buttons.push(button);
    }

    let ui = MoreActionsStuff {
        main_view,
        panel_view,
        title,
        close_button,
        buttons,
    };
    layout_more_actions(env, &ui, app_frame.size, orientation);
    ui
}

fn keep_more_actions_above_picker(
    env: &mut Environment,
    picker_view: id,
    ui: &MoreActionsStuff,
    is_open: bool,
) {
    if is_open {
        () = msg![env; picker_view bringSubviewToFront:(ui.main_view)];
    }
}

struct CloudSyncSettingsStuff {
    main_view: id,
    title: id,
    status_text_view: id,
    connect_button: id,
    sync_label: id,
    sync_switch: id,
    back_button: id,
}

#[derive(Clone, Copy)]
struct CloudSyncSettingsFrames {
    title: CGRect,
    status: CGRect,
    connect: CGRect,
    sync_label: CGRect,
    sync_switch: CGRect,
    back: CGRect,
}

fn cloud_sync_settings_frames(
    size: CGSize,
    orientation: DeviceOrientation,
) -> CloudSyncSettingsFrames {
    let is_landscape = matches!(
        orientation,
        DeviceOrientation::LandscapeLeft | DeviceOrientation::LandscapeRight
    );
    let margin = 12.0;
    let title_y = if is_landscape { 8.0 } else { 16.0 };
    let title_height = 32.0;
    let status_y = title_y + title_height + 8.0;
    let control_center = size.height - 80.0;
    let status_bottom = control_center - 28.0;
    let status_height = (status_bottom - status_y).max(60.0);
    let connect_width = (size.width - margin * 2.0).min(260.0).max(0.0);
    let connect_height = 36.0;
    let back_width = (size.width - margin * 2.0).min(160.0).max(0.0);
    let back_height = 36.0;

    CloudSyncSettingsFrames {
        title: CGRect {
            origin: CGPoint {
                x: margin,
                y: title_y,
            },
            size: CGSize {
                width: (size.width - margin * 2.0).max(0.0),
                height: title_height,
            },
        },
        status: CGRect {
            origin: CGPoint {
                x: margin,
                y: status_y,
            },
            size: CGSize {
                width: (size.width - margin * 2.0).max(0.0),
                height: status_height,
            },
        },
        connect: CGRect {
            origin: CGPoint {
                x: (size.width - connect_width) / 2.0,
                y: control_center - connect_height / 2.0,
            },
            size: CGSize {
                width: connect_width,
                height: connect_height,
            },
        },
        sync_label: CGRect {
            origin: CGPoint {
                x: margin,
                y: control_center - 18.0,
            },
            size: CGSize {
                width: (size.width - UI_SWITCH_WIDTH - margin * 3.0).max(0.0),
                height: 36.0,
            },
        },
        sync_switch: CGRect {
            origin: CGPoint {
                x: (size.width - margin - UI_SWITCH_WIDTH).max(0.0),
                y: control_center - 13.5,
            },
            size: CGSize {
                width: UI_SWITCH_WIDTH,
                height: 27.0,
            },
        },
        back: CGRect {
            origin: CGPoint {
                x: (size.width - back_width) / 2.0,
                y: size.height - 28.0 - back_height / 2.0,
            },
            size: CGSize {
                width: back_width,
                height: back_height,
            },
        },
    }
}

fn layout_cloud_sync_settings(
    env: &mut Environment,
    ui: &CloudSyncSettingsStuff,
    size: CGSize,
    orientation: DeviceOrientation,
) {
    let frames = cloud_sync_settings_frames(size, orientation);
    () = msg![env; (ui.main_view) setFrame:(CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size,
    })];
    () = msg![env; (ui.title) setFrame:(frames.title)];
    () = msg![env; (ui.status_text_view) setFrame:(frames.status)];
    () = msg![env; (ui.connect_button) setFrame:(frames.connect)];
    () = msg![env; (ui.connect_button) layoutSubviews];
    () = msg![env; (ui.sync_label) setFrame:(frames.sync_label)];
    () = msg![env; (ui.sync_switch) setFrame:(frames.sync_switch)];
    () = msg![env; (ui.sync_switch) layoutSubviews];
    () = msg![env; (ui.back_button) setFrame:(frames.back)];
    () = msg![env; (ui.back_button) layoutSubviews];
}

fn keep_cloud_sync_settings_above_picker(
    env: &mut Environment,
    picker_view: id,
    ui: &CloudSyncSettingsStuff,
    is_open: bool,
) {
    if is_open {
        () = msg![env; picker_view bringSubviewToFront:(ui.main_view)];
    }
}

fn setup_cloud_sync_settings(
    env: &mut Environment,
    delegate: id,
    super_view: id,
    app_frame: CGRect,
    orientation: DeviceOrientation,
) -> CloudSyncSettingsStuff {
    let main_view: id = msg_class![env; UIView alloc];
    let main_view: id = msg![env; main_view initWithFrame:(CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: app_frame.size,
    })];
    let background: id = msg_class![env; UIColor whiteColor];
    () = msg![env; main_view setBackgroundColor:background];
    () = msg![env; main_view setHidden:true];
    () = msg![env; super_view addSubview:main_view];

    let title: id = msg_class![env; UILabel alloc];
    let title: id = msg![env; title initWithFrame:(CGRect::default())];
    let title_text = ns_string::get_static_str(env, "Google Drive Sync");
    () = msg![env; title setText:title_text];
    let title_font: id = msg_class![env; UIFont boldSystemFontOfSize:(22.0 as CGFloat)];
    () = msg![env; title setFont:title_font];
    let title_color: id = msg_class![env; UIColor blackColor];
    () = msg![env; title setTextColor:title_color];
    () = msg![env; main_view addSubview:title];

    let status_text_view: id = msg_class![env; UITextView alloc];
    let status_text_view: id = msg![env; status_text_view initWithFrame:(CGRect::default())];
    () = msg![env; status_text_view setEditable:false];
    let status_background: id = msg_class![env; UIColor whiteColor];
    () = msg![env; status_text_view setBackgroundColor:status_background];
    let status_color: id = msg_class![env; UIColor blackColor];
    () = msg![env; status_text_view setTextColor:status_color];
    let body_font: id = msg_class![env; UIFont systemFontOfSize:(12.0 as CGFloat)];
    () = msg![env; status_text_view setFont:body_font];
    () = msg![env; main_view addSubview:status_text_view];

    let connect_button = make_button_row(
        env,
        delegate,
        main_view,
        app_frame.size,
        app_frame.size.height - 80.0,
        &[("Connect Google Drive", "syncConnect")],
        None,
    )[0];
    let connect_title: id = msg![env; connect_button titleLabel];
    let connect_font: id = msg_class![env; UIFont systemFontOfSize:(14.0 as CGFloat)];
    () = msg![env; connect_title setFont:connect_font];
    let sync_label: id = msg_class![env; UILabel alloc];
    let sync_label: id = msg![env; sync_label initWithFrame:(CGRect::default())];
    let sync_label_text = ns_string::get_static_str(env, "Enable Google Drive sync");
    () = msg![env; sync_label setText:sync_label_text];
    let sync_label_font: id = msg_class![env; UIFont systemFontOfSize:(14.0 as CGFloat)];
    () = msg![env; sync_label setFont:sync_label_font];
    () = msg![env; sync_label setHidden:true];
    () = msg![env; main_view addSubview:sync_label];

    let sync_switch: id = msg_class![env; UISwitch alloc];
    let sync_switch: id = msg![env; sync_switch initWithFrame:(CGRect::default())];
    () = msg![env; sync_switch setOn:false];
    () = msg![env; sync_switch setHidden:true];
    let selector = env.objc.lookup_selector("syncToggle:").unwrap();
    () = msg![env; sync_switch addTarget:delegate
                                    action:selector
                          forControlEvents:UIControlEventValueChanged];
    () = msg![env; main_view addSubview:sync_switch];

    let back_button = make_button_row(
        env,
        delegate,
        main_view,
        app_frame.size,
        app_frame.size.height - 28.0,
        &[("Back", "syncSettingsHide")],
        None,
    )[0];
    let back_title: id = msg![env; back_button titleLabel];
    let back_font: id = msg_class![env; UIFont systemFontOfSize:(14.0 as CGFloat)];
    () = msg![env; back_title setFont:back_font];

    let ui = CloudSyncSettingsStuff {
        main_view,
        title,
        status_text_view,
        connect_button,
        sync_label,
        sync_switch,
        back_button,
    };
    layout_cloud_sync_settings(env, &ui, app_frame.size, orientation);
    ui
}

fn picker_live_status(root: &Path, has_control: bool, current: Option<&LiveStatus>) -> LiveStatus {
    let mut status = load_live_status(root).unwrap_or_else(|_| {
        current.cloned().unwrap_or_else(|| LiveStatus {
            error: Some("local status unavailable".into()),
            ..Default::default()
        })
    });
    if !has_control {
        status.active = false;
    }
    status
}

fn redacted_auth_error(error: &auth::AuthError) -> &'static str {
    match error {
        auth::AuthError::MissingCredentials => "Google Drive is not connected",
        auth::AuthError::ReauthorizationRequired => "Reconnect Google Drive to authorize sync",
        auth::AuthError::ForegroundRequired => "Google authorization requires foreground approval",
        auth::AuthError::Cancelled => "Google authorization was cancelled",
        auth::AuthError::InvalidResponse => "Google returned an incomplete token response",
        auth::AuthError::Storage(_) => "Secure credential storage failed",
        auth::AuthError::OAuth(_) => "Google authorization failed",
    }
}

fn format_live_status(settings: &SyncSettings, status: &LiveStatus) -> String {
    let state = if !settings.enabled {
        "Disabled"
    } else if status.error.is_some() {
        "Enabled; local observer needs attention"
    } else if status.active {
        "Enabled; local observer active"
    } else {
        "Enabled; observer unavailable"
    };
    let observer = if status.active {
        "Active"
    } else {
        "Unavailable"
    };
    let background = match status.background_sync {
        crate::sync::status::BackgroundSyncState::Idle => "Not scheduled",
        crate::sync::status::BackgroundSyncState::Running => "Running",
        crate::sync::status::BackgroundSyncState::Deferred => "Deferred until game exits",
        crate::sync::status::BackgroundSyncState::NeedsAuthorization => "Reconnect Google Drive",
        crate::sync::status::BackgroundSyncState::Completed => "Complete",
        crate::sync::status::BackgroundSyncState::Conflict => "Needs conflict resolution",
        crate::sync::status::BackgroundSyncState::Failed => "Failed",
    };
    let timestamp =
        |value: Option<i64>| value.map_or_else(|| "Never".to_owned(), format_unix_ms_local);
    let error = match status.error.as_deref() {
        None => "None",
        Some("sync conflicts require resolution") | Some("sync conflicts: resolution required") => {
            "Unresolved conflict; local files preserved"
        }
        Some("filesystem observer unavailable; using periodic reconciliation")
        | Some("filesystem observer failed; using periodic reconciliation") => {
            "Filesystem observer unavailable; using periodic reconciliation"
        }
        Some("sync provider failure: ConfigInvalid")
        | Some("prepare Google Drive folder: ConfigInvalid") => {
            "ConfigInvalid; Google Drive reconciliation not confirmed"
        }
        Some("cloud is unavailable") => "Google Drive unavailable; local files preserved",
        Some("cloud backup unavailable: device identity not persisted") => {
            "Google Drive backup unavailable; local device identity could not be persisted"
        }
        Some("local status unavailable") => "Local status unavailable",
        Some(_) => "Sync needs attention; details redacted",
    };
    format!(
        "Google Drive sync: {state}\nObserver: {observer}\nLast reconciliation: {background}\nLocal changes to rescan: {}\nLast full sync: {}\nLatest error: {error}",
        status.queued,
        timestamp(status.last_full_sync_unix_ms),
    )
}

fn update_cloud_sync_settings(
    env: &mut Environment,
    ui: &CloudSyncSettingsStuff,
    settings: &SyncSettings,
    status: &LiveStatus,
    account_connected: bool,
    auth_status: &str,
) {
    let account = if account_connected {
        "Connected"
    } else {
        "Not connected"
    };
    let client = if auth::authorization_available() {
        ""
    } else {
        "\nThis build has no Google OAuth client ID."
    };
    let detail = format!(
        "Account: {account}{client}\n{}\nGoogle permission: access to files created or opened with touchHLE. Sync data stays in the touchHLE folder.\n\n{auth_status}",
        format_live_status(settings, status)
    );
    let detail = ns_string::from_rust_string(env, detail);
    let previous_offset: CGPoint = msg![env; (ui.status_text_view) contentOffset];
    () = msg![env; (ui.status_text_view) setText:detail];
    let content_size: CGSize = msg![env; (ui.status_text_view) contentSize];
    let bounds: CGRect = msg![env; (ui.status_text_view) bounds];
    () = msg![env; (ui.status_text_view) setContentOffset:(CGPoint {
        x: 0.0,
        y: clamped_status_offset(previous_offset.y, content_size.height, bounds.size.height),
    })];
    let hide_sync_controls = !account_connected;
    let sync_enabled = settings.enabled;
    () = msg![env; (ui.connect_button) setHidden:account_connected];
    () = msg![env; (ui.sync_label) setHidden:hide_sync_controls];
    () = msg![env; (ui.sync_switch) setHidden:hide_sync_controls];
    () = msg![env; (ui.sync_switch) setOn:sync_enabled];
}

fn clamped_status_offset(
    previous: CGFloat,
    content_height: CGFloat,
    viewport_height: CGFloat,
) -> CGFloat {
    previous
        .max(0.0)
        .min((content_height - viewport_height).max(0.0))
}

struct CopyrightInfoStuff {
    main_view: id,
    title: id,
    text_frame: CGRect,
    text_label: id,
    font: id,
    pages: Vec<(std::ops::Range<usize>, CGFloat)>,
    last_page_idx: Option<usize>,
    prev_page_button: id,
    next_page_button: id,
    close_button: id,
}

#[derive(Clone, Copy)]
struct CopyrightInfoFrames {
    title: CGRect,
    text: CGRect,
    previous: CGRect,
    next: CGRect,
    close: CGRect,
}

fn copyright_info_frames(size: CGSize, orientation: DeviceOrientation) -> CopyrightInfoFrames {
    let is_landscape = matches!(
        orientation,
        DeviceOrientation::LandscapeLeft | DeviceOrientation::LandscapeRight
    );
    let margin = 12.0;
    let title_y = if is_landscape { 8.0 } else { 16.0 };
    let title_height = 30.0;
    let text_y = title_y + title_height + 8.0;
    let nav_y = size.height - 48.0;
    let nav_height = 36.0;
    let nav_width = (size.width - margin * 2.0).max(0.0) / 3.0;

    CopyrightInfoFrames {
        title: CGRect {
            origin: CGPoint {
                x: margin,
                y: title_y,
            },
            size: CGSize {
                width: (size.width - margin * 2.0).max(0.0),
                height: title_height,
            },
        },
        text: CGRect {
            origin: CGPoint {
                x: margin,
                y: text_y,
            },
            size: CGSize {
                width: (size.width - margin * 2.0).max(0.0),
                height: (nav_y - text_y - 8.0).max(0.0),
            },
        },
        previous: CGRect {
            origin: CGPoint {
                x: margin,
                y: nav_y,
            },
            size: CGSize {
                width: nav_width,
                height: nav_height,
            },
        },
        next: CGRect {
            origin: CGPoint {
                x: margin + nav_width,
                y: nav_y,
            },
            size: CGSize {
                width: nav_width,
                height: nav_height,
            },
        },
        close: CGRect {
            origin: CGPoint {
                x: margin + nav_width * 2.0,
                y: nav_y,
            },
            size: CGSize {
                width: nav_width,
                height: nav_height,
            },
        },
    }
}

fn layout_copyright_info(
    env: &mut Environment,
    ui: &mut CopyrightInfoStuff,
    size: CGSize,
    orientation: DeviceOrientation,
) {
    let frames = copyright_info_frames(size, orientation);
    if ui.text_frame != frames.text {
        ui.pages.clear();
        ui.last_page_idx = None;
    }
    () = msg![env; (ui.main_view) setFrame:(CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size,
    })];
    () = msg![env; (ui.title) setFrame:(frames.title)];
    () = msg![env; (ui.text_label) setFrame:(frames.text)];
    () = msg![env; (ui.prev_page_button) setFrame:(frames.previous)];
    () = msg![env; (ui.next_page_button) setFrame:(frames.next)];
    () = msg![env; (ui.close_button) setFrame:(frames.close)];
    () = msg![env; (ui.prev_page_button) layoutSubviews];
    () = msg![env; (ui.next_page_button) layoutSubviews];
    () = msg![env; (ui.close_button) layoutSubviews];
    ui.text_frame = frames.text;
}

fn keep_copyright_info_above_picker(
    env: &mut Environment,
    picker_view: id,
    ui: &CopyrightInfoStuff,
    is_open: bool,
) {
    if is_open {
        () = msg![env; picker_view bringSubviewToFront:(ui.main_view)];
    }
}

fn setup_copyright_info(
    env: &mut Environment,
    delegate: id,
    super_view: id,
    app_frame: CGRect,
    orientation: DeviceOrientation,
) -> CopyrightInfoStuff {
    let main_frame = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: app_frame.size,
    };

    let frames = copyright_info_frames(main_frame.size, orientation);

    // Container for all the other stuff

    let main_view: id = msg_class![env; UIView alloc];
    let main_view: id = msg![env; main_view initWithFrame:main_frame];
    // TODO: Isn't white the default?
    let bg_color: id = msg_class![env; UIColor whiteColor];
    () = msg![env; main_view setBackgroundColor:bg_color];
    // This main_view is hidden until the copyright info button is tapped.
    () = msg![env; main_view setHidden:true];
    () = msg![env; super_view addSubview:main_view];

    // UILabel that will display part of the copyright text

    let title: id = msg_class![env; UILabel alloc];
    let title: id = msg![env; title initWithFrame:(frames.title)];
    let title_text = ns_string::get_static_str(env, "Copyright info");
    () = msg![env; title setText:title_text];
    let title_font: id = msg_class![env; UIFont boldSystemFontOfSize:(18.0 as CGFloat)];
    () = msg![env; title setFont:title_font];
    let title_color: id = msg_class![env; UIColor blackColor];
    () = msg![env; title setTextColor:title_color];
    () = msg![env; main_view addSubview:title];

    let text_frame = frames.text;

    let text_label: id = msg_class![env; UILabel alloc];
    let text_label: id = msg![env; text_label initWithFrame:text_frame];
    () = msg![env; text_label setNumberOfLines:0]; // unlimited
    let text_color: id = msg_class![env; UIColor blackColor];
    () = msg![env; text_label setTextColor:text_color];
    let bg_color: id = msg_class![env; UIColor clearColor];
    () = msg![env; text_label setBackgroundColor:bg_color];
    let font_size: CGFloat = 14.0;
    let font: id = msg_class![env; UIFont systemFontOfSize:font_size];
    () = msg![env; text_label setFont:font];
    () = msg![env; main_view addSubview:text_label];

    // Navigation

    let buttons = make_button_row(
        env,
        delegate,
        main_view,
        main_frame.size,
        main_frame.size.height - 30.0,
        &[
            ("Previous", "copyrightInfoPrevPage"),
            ("Next", "copyrightInfoNextPage"),
            ("Back", "copyrightInfoHide"),
        ],
        Some(12.0),
    );

    let mut ui = CopyrightInfoStuff {
        main_view,
        title,
        text_frame,
        text_label,
        font,
        pages: Vec::new(),
        last_page_idx: None,
        prev_page_button: buttons[0],
        next_page_button: buttons[1],
        close_button: buttons[2],
    };
    layout_copyright_info(env, &mut ui, main_frame.size, orientation);
    ui
}

fn change_copyright_page(
    env: &mut Environment,
    copyright_info_stuff: &mut CopyrightInfoStuff,
    copyright_info_text: &str,
    page_idx: usize,
) {
    // TODO: Eventually this should be ripped out and replaced with a scrolling
    // UITextView, once that's implemented.

    let &mut CopyrightInfoStuff {
        text_frame,
        text_label,
        font,
        ref mut pages,
        ref mut last_page_idx,
        prev_page_button,
        next_page_button,
        ..
    } = copyright_info_stuff;

    // Lazily lay out pages of text as needed.

    if page_idx == pages.len() {
        let mut page_start = pages.last().map_or(0, |page| page.0.end);
        while copyright_info_text[page_start..].starts_with([' ', '\n', '\r']) {
            page_start += 1;
        }
        let mut page_height = 0.0;
        let page_end = loop {
            let mut line_start = page_start;
            while line_start < copyright_info_text.len() {
                let is_first_line = line_start == page_start;

                let line_end = if let Some(i) = copyright_info_text[line_start..].find('\n') {
                    line_start + i + 1
                } else {
                    copyright_info_text.len()
                };

                let line = &copyright_info_text[line_start..line_end];

                // Force pagination before headings (in Dynarmic's license text)
                if !is_first_line && line.starts_with("###") {
                    break;
                }

                let line_temp = ns_string::from_rust_string(env, line.to_string());
                let line_size: CGSize = msg![env; line_temp sizeWithFont:font
                                                       constrainedToSize:(text_frame.size)];
                // Avoid accumulation of old line strings.
                release(env, line_temp);

                if page_height + line_size.height > text_frame.size.height {
                    break;
                }

                page_height += line_size.height;
                line_start = line_end;

                // Force pagination after dividers
                if !is_first_line && line.starts_with("---") {
                    break;
                }
            }
            let page_end = line_start;
            assert!(page_start != page_end);

            // Avoid entirely blank pages
            if copyright_info_text[page_start..page_end].trim() == "" {
                page_start = page_end;
            } else {
                break page_end;
            }
        };
        assert!(page_start != page_end);
        pages.push((page_start..page_end, page_height));
        if page_end == copyright_info_text.len() {
            *last_page_idx = Some(page_idx);
        }
    }

    // Actually display the page

    let (page, page_height) = pages[page_idx].clone();
    let page = &copyright_info_text[page];

    let page: id = ns_string::from_rust_string(env, page.to_string());
    () = msg![env; text_label setText:page];
    // Avoid accumulation of old page strings.
    release(env, page);

    // UILabel always vertically centers text. Work around that by resizing it.
    let label_frame = CGRect {
        origin: text_frame.origin,
        size: CGSize {
            width: text_frame.size.width,
            // The page height is slightly off, a little padding is needed.
            height: page_height + 10.0,
        },
    };
    () = msg![env; text_label setFrame:label_frame];

    () = msg![env; prev_page_button setHidden:(page_idx == 0)];
    () = msg![env; next_page_button setHidden:(Some(page_idx) == *last_page_idx)];
}

struct QuickOptionsStuff {
    main_view: id,
    title: id,
    close_button: id,
    row_labels: Vec<id>,
    row_switches: Vec<id>,
    scale_hack_buttons: [id; 5],
    orientation_buttons: [id; 4],
}

fn layout_quick_option_button_row(
    env: &mut Environment,
    buttons: &[id],
    size: CGSize,
    y: CGFloat,
    height: CGFloat,
    font_size: CGFloat,
) {
    let margin = 12.0;
    let gap = 4.0;
    let button_width = ((size.width - margin * 2.0 - gap * (buttons.len() as CGFloat - 1.0))
        / buttons.len() as CGFloat)
        .max(0.0);
    for (index, &button) in buttons.iter().enumerate() {
        let frame = CGRect {
            origin: CGPoint {
                x: margin + index as CGFloat * (button_width + gap),
                y,
            },
            size: CGSize {
                width: button_width,
                height,
            },
        };
        () = msg![env; button setFrame:frame];
        let label: id = msg![env; button titleLabel];
        let font: id = msg_class![env; UIFont systemFontOfSize:font_size];
        () = msg![env; label setFont:font];
        () = msg![env; button layoutSubviews];
    }
}

fn layout_quick_options(
    env: &mut Environment,
    ui: &QuickOptionsStuff,
    size: CGSize,
    orientation: DeviceOrientation,
) {
    let is_landscape = matches!(
        orientation,
        DeviceOrientation::LandscapeLeft | DeviceOrientation::LandscapeRight
    );
    let overlay_frame = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size,
    };
    () = msg![env; (ui.main_view) setFrame:overlay_frame];
    () = msg![env; (ui.title) setFrame:(CGRect {
        origin: CGPoint {
            x: 12.0,
            y: if is_landscape { 8.0 } else { 14.0 },
        },
        size: CGSize {
            width: (size.width - 64.0).max(0.0),
            height: 32.0,
        },
    })];
    let title_font: id = msg_class![env; UIFont boldSystemFontOfSize:(20.0 as CGFloat)];
    () = msg![env; (ui.title) setFont:title_font];
    () = msg![env; (ui.close_button) setFrame:(CGRect {
        origin: CGPoint {
            x: (size.width - 48.0).max(0.0),
            y: if is_landscape { 4.0 } else { 10.0 },
        },
        size: CGSize {
            width: 36.0,
            height: 36.0,
        },
    })];
    () = msg![env; (ui.close_button) layoutSubviews];

    let (scale_label_y, scale_buttons_y, orientation_label_y, orientation_buttons_y) =
        if is_landscape {
            (38.0, 62.0, 102.0, 126.0)
        } else {
            (54.0, 76.0, 132.0, 154.0)
        };
    let row_label_font: id = msg_class![env; UIFont systemFontOfSize:(14.0 as CGFloat)];
    for (index, &label) in ui.row_labels.iter().enumerate() {
        let frame = if index == 0 {
            CGRect {
                origin: CGPoint {
                    x: 12.0,
                    y: scale_label_y,
                },
                size: CGSize {
                    width: (size.width - 24.0).max(0.0),
                    height: 22.0,
                },
            }
        } else if index == 1 {
            CGRect {
                origin: CGPoint {
                    x: 12.0,
                    y: orientation_label_y,
                },
                size: CGSize {
                    width: (size.width - 24.0).max(0.0),
                    height: 22.0,
                },
            }
        } else {
            let toggle_index = index - 2;
            let center_y = if is_landscape {
                180.0 + toggle_index as CGFloat * 44.0
            } else {
                240.0 + toggle_index as CGFloat * 58.0
            };
            CGRect {
                origin: CGPoint {
                    x: 12.0,
                    y: center_y - 18.0,
                },
                size: CGSize {
                    width: (size.width - 130.0).max(0.0),
                    height: 36.0,
                },
            }
        };
        () = msg![env; label setFrame:frame];
        () = msg![env; label setFont:row_label_font];
        () = msg![env; label setNumberOfLines:(if index >= 3 { 2 } else { 1 })];
    }

    let button_height = if is_landscape { 36.0 } else { 38.0 };
    layout_quick_option_button_row(
        env,
        &ui.scale_hack_buttons,
        size,
        scale_buttons_y,
        button_height,
        12.0,
    );
    layout_quick_option_button_row(
        env,
        &ui.orientation_buttons,
        size,
        orientation_buttons_y,
        button_height,
        12.0,
    );
    for (index, &switch) in ui.row_switches.iter().enumerate() {
        let center_y = if is_landscape {
            180.0 + index as CGFloat * 44.0
        } else {
            240.0 + index as CGFloat * 58.0
        };
        () = msg![env; switch setFrame:(CGRect {
            origin: CGPoint {
                x: (size.width - 12.0 - UI_SWITCH_WIDTH).max(0.0),
                y: center_y - 13.5,
            },
            size: CGSize {
                width: UI_SWITCH_WIDTH,
                height: 27.0,
            },
        })];
        () = msg![env; switch layoutSubviews];
    }
}

fn keep_quick_options_above_picker(
    env: &mut Environment,
    picker_view: id,
    ui: &QuickOptionsStuff,
    is_open: bool,
) {
    if is_open {
        () = msg![env; picker_view bringSubviewToFront:(ui.main_view)];
    }
}

fn setup_quick_options(
    env: &mut Environment,
    delegate: id,
    super_view: id,
    app_frame: CGRect,
    orientation: DeviceOrientation,
) -> QuickOptionsStuff {
    // UIView*
    let main_frame = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: app_frame.size,
    };

    // Container for all the other stuff

    let main_view: id = msg_class![env; UIView alloc];
    let main_view: id = msg![env; main_view initWithFrame:main_frame];
    // TODO: Isn't white the default?
    let bg_color: id = msg_class![env; UIColor whiteColor];
    () = msg![env; main_view setBackgroundColor:bg_color];
    // This main_view is hidden until the copyright info button is tapped.
    () = msg![env; main_view setHidden:true];
    () = msg![env; super_view addSubview:main_view];

    let divider = 40.0;

    let title: id = msg_class![env; UILabel alloc];
    let title: id = msg![env; title initWithFrame:(CGRect::default())];
    let title_text = ns_string::get_static_str(env, "Quick Options");
    () = msg![env; title setText:title_text];
    let title_color: id = msg_class![env; UIColor blackColor];
    () = msg![env; title setTextColor:title_color];
    () = msg![env; main_view addSubview:title];

    let close_button: id = msg_class![env; UIButton buttonWithType:UIButtonTypeRoundedRect];
    let close_text = ns_string::get_static_str(env, "×");
    () = msg![env; close_button setTitle:close_text forState:UIControlStateNormal];
    let close_selector = env.objc.lookup_selector("quickOptionsHide").unwrap();
    () = msg![env; close_button addTarget:delegate
                                   action:close_selector
                         forControlEvents:UIControlEventTouchUpInside];
    () = msg![env; main_view addSubview:close_button];

    enum RowKind {
        Label(&'static str),
        Buttons(&'static [(&'static str, &'static str)]),
        Switch(&'static str, bool),
    }
    let rows = [
        RowKind::Label("Scale hack"),
        RowKind::Buttons(&[
            ("Default", "scaleHackDefault"),
            ("Off", "scaleHack1"),
            ("2×", "scaleHack2"),
            ("3×", "scaleHack3"),
            ("4×", "scaleHack4"),
        ]),
        RowKind::Label("Orientation"),
        RowKind::Buttons(&[
            ("Default", "orientationDefault"),
            ("←", "orientationLandscapeLeft"),
            ("→", "orientationLandscapeRight"),
            ("↓", "orientationPortraitUpsideDown"),
        ]),
        RowKind::Label("Network access"),
        RowKind::Switch("network:", false),
        RowKind::Label("Use analog sticks for tilt controls"),
        RowKind::Switch("analogStickTiltControls:", true),
        // ---- (divider for stuff skipped below)
        RowKind::Label("Fullscreen (override)"),
        RowKind::Switch("fullscreen:", false),
    ];
    let rows_len_full = rows.len();
    let rows = if crate::window::Window::rotatable_fullscreen() {
        // Fullscreen option doesn't make sense on always-fullscreen platforms
        &rows[..rows.len() - 2]
    } else {
        &rows[..]
    };

    let mut button_rows = Vec::new();
    let mut row_labels = Vec::new();
    let mut row_switches = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let row_center = divider
            + ((1 + i) as CGFloat)
                * ((main_frame.size.height - divider) / ((rows_len_full + 1) as CGFloat));

        match *row {
            RowKind::Label(text) => {
                let frame = CGRect {
                    origin: CGPoint {
                        x: 0.0,
                        y: row_center - 30.0 / 2.0,
                    },
                    size: CGSize {
                        width: main_frame.size.width,
                        height: 30.0,
                    },
                };

                let label: id = msg_class![env; UILabel alloc];
                let label: id = msg![env; label initWithFrame:frame];
                let text = ns_string::get_static_str(env, text);
                () = msg![env; label setText:text];
                () = msg![env; label setTextAlignment:UITextAlignmentLeft];
                () = msg![env; main_view addSubview:label];
                row_labels.push(label);
            }
            RowKind::Buttons(buttons) => {
                button_rows.push(make_button_row(
                    env,
                    delegate,
                    main_view,
                    main_frame.size,
                    row_center,
                    buttons,
                    /* font_size: */ None,
                ));
            }
            RowKind::Switch(selector, default_state) => {
                let switch_frame = CGRect {
                    origin: CGPoint {
                        x: main_frame.size.width / 2.0 - 94.0 / 2.0,
                        y: row_center - 27.0 / 2.0,
                    },
                    size: Default::default(),
                };

                let switch: id = msg_class![env; UISwitch alloc];
                let switch: id = msg![env; switch initWithFrame:switch_frame];
                () = msg![env; switch setOn:default_state];
                let selector = env.objc.lookup_selector(selector).unwrap();
                () = msg![env; switch addTarget:delegate
                                         action:selector
                               forControlEvents:UIControlEventValueChanged];
                () = msg![env; main_view addSubview:switch];
                row_switches.push(switch);
            }
        }
    }

    let ui = QuickOptionsStuff {
        main_view,
        title,
        close_button,
        row_labels,
        row_switches,
        scale_hack_buttons: button_rows[0][..].try_into().unwrap(),
        orientation_buttons: button_rows[1][..].try_into().unwrap(),
    };
    layout_quick_options(env, &ui, main_frame.size, orientation);
    ui
}

#[cfg(test)]
mod cloud_sync_ui_tests {
    use super::*;
    use crate::sync::status::LiveStatus;
    use chrono::TimeZone;

    #[test]
    fn enabling_cloud_sync_requests_an_immediate_sync() {
        let mut requested = false;
        request_sync_if_enabled(&mut requested, false);
        assert!(!requested);
        request_sync_if_enabled(&mut requested, true);
        assert!(requested);
    }

    #[test]
    fn status_redraw_preserves_and_clamps_scroll_instead_of_jumping_to_bottom() {
        assert_eq!(clamped_status_offset(22.0, 200.0, 100.0), 22.0);
        assert_eq!(clamped_status_offset(120.0, 140.0, 100.0), 40.0);
        assert_eq!(clamped_status_offset(-5.0, 80.0, 100.0), 0.0);
        assert_eq!(clamped_status_offset(5.0, 80.0, 100.0), 0.0);
    }

    #[test]
    fn cloud_sync_settings_layout_fits_portrait_and_landscape_without_overlap() {
        for (size, orientation) in [
            (
                CGSize {
                    width: 320.0,
                    height: 480.0,
                },
                DeviceOrientation::Portrait,
            ),
            (
                CGSize {
                    width: 480.0,
                    height: 320.0,
                },
                DeviceOrientation::LandscapeLeft,
            ),
            (
                CGSize {
                    width: 480.0,
                    height: 320.0,
                },
                DeviceOrientation::LandscapeRight,
            ),
        ] {
            let frames = cloud_sync_settings_frames(size, orientation);
            assert!(frames.status.origin.y + frames.status.size.height <= frames.connect.origin.y);
            assert!(frames.connect.origin.y + frames.connect.size.height <= frames.back.origin.y);
            for frame in [
                frames.title,
                frames.status,
                frames.connect,
                frames.sync_label,
                frames.sync_switch,
                frames.back,
            ] {
                assert!(frame.origin.x >= 0.0);
                assert!(frame.origin.y >= 0.0);
                assert!(frame.origin.x + frame.size.width <= size.width);
                assert!(frame.origin.y + frame.size.height <= size.height);
            }
        }
    }

    #[test]
    fn quick_options_layout_keeps_controls_inside_portrait_and_landscape_screens() {
        for (size, orientation) in [
            (
                CGSize {
                    width: 320.0,
                    height: 480.0,
                },
                DeviceOrientation::Portrait,
            ),
            (
                CGSize {
                    width: 480.0,
                    height: 320.0,
                },
                DeviceOrientation::LandscapeLeft,
            ),
            (
                CGSize {
                    width: 480.0,
                    height: 320.0,
                },
                DeviceOrientation::LandscapeRight,
            ),
        ] {
            let is_landscape = matches!(
                orientation,
                DeviceOrientation::LandscapeLeft | DeviceOrientation::LandscapeRight
            );
            let (scale_y, scale_buttons_y, orientation_y, orientation_buttons_y) = if is_landscape {
                (38.0, 62.0, 102.0, 126.0)
            } else {
                (54.0, 76.0, 132.0, 154.0)
            };
            assert!(scale_y + 22.0 <= scale_buttons_y);
            assert!(scale_buttons_y + if is_landscape { 36.0 } else { 38.0 } <= orientation_y);
            assert!(orientation_y + 22.0 <= orientation_buttons_y);
            let switch_count = 3;
            let last_switch_center = if is_landscape {
                182.0 + (switch_count - 1) as CGFloat * 44.0
            } else {
                240.0 + (switch_count - 1) as CGFloat * 58.0
            };
            assert!(last_switch_center + 18.0 <= size.height);
        }
    }

    #[test]
    fn copyright_layout_reserves_a_fixed_navigation_row_in_both_orientations() {
        for (size, orientation) in [
            (
                CGSize {
                    width: 320.0,
                    height: 480.0,
                },
                DeviceOrientation::Portrait,
            ),
            (
                CGSize {
                    width: 480.0,
                    height: 320.0,
                },
                DeviceOrientation::LandscapeLeft,
            ),
        ] {
            let frames = copyright_info_frames(size, orientation);
            assert!(frames.text.origin.y + frames.text.size.height < frames.previous.origin.y);
            for frame in [
                frames.title,
                frames.text,
                frames.previous,
                frames.next,
                frames.close,
            ] {
                assert!(frame.origin.x >= 0.0);
                assert!(frame.origin.y >= 0.0);
                assert!(frame.origin.x + frame.size.width <= size.width);
                assert!(frame.origin.y + frame.size.height <= size.height);
            }
            let previous_y = frames.previous.origin.y;
            let next_y = frames.next.origin.y;
            let close_y = frames.close.origin.y;
            assert_eq!(previous_y, next_y);
            assert_eq!(next_y, close_y);
        }
    }

    #[test]
    fn settings_keep_in_memory_status_when_status_file_is_unavailable() {
        let root =
            std::env::temp_dir().join(format!("touchhle-picker-status-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(paths::SYNC_DIR).join("live-status.json")).unwrap();
        let current = LiveStatus {
            active: true,
            queued: 2,
            error: Some("cloud backup unavailable: device identity not persisted".into()),
            ..Default::default()
        };
        assert_eq!(picker_live_status(&root, true, Some(&current)), current);
        assert!(!picker_live_status(&root, false, Some(&current)).active);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_status_presentation_covers_disabled_and_active_queue() {
        let status = LiveStatus {
            active: true,
            queued: 3,
            ..Default::default()
        };
        let disabled = format_live_status(&SyncSettings { enabled: false }, &status);
        assert!(disabled.contains("Disabled"));
        assert!(!disabled.contains("local observer active"));
        let enabled = format_live_status(&SyncSettings { enabled: true }, &status);
        assert!(enabled.contains("Google Drive sync:"));
        assert!(!enabled.contains("Cloud sync:"));
        assert!(enabled.contains("local observer active"));
        assert!(enabled.contains("Local changes to rescan: 3"));
        assert!(!enabled.contains("Queued:"));
    }

    #[test]
    fn live_status_presentation_reports_background_sync_state() {
        let status = LiveStatus {
            background_sync: crate::sync::status::BackgroundSyncState::Running,
            ..Default::default()
        };

        assert!(format_live_status(&SyncSettings { enabled: true }, &status)
            .contains("Last reconciliation: Running"));

        let failed = LiveStatus {
            background_sync: crate::sync::status::BackgroundSyncState::Failed,
            ..Default::default()
        };
        let failed_text = format_live_status(&SyncSettings { enabled: true }, &failed);
        assert!(failed_text.contains("Last reconciliation: Failed"));
        assert!(!failed_text.contains("will retry"));
    }

    #[test]
    fn live_status_presentation_distinguishes_failures_from_success() {
        let settings = SyncSettings { enabled: true };
        let failed = LiveStatus {
            active: false,
            queued: 1,
            last_upload_unix_ms: Some(0),
            error: Some("sync provider failure: ConfigInvalid".into()),
            ..Default::default()
        };
        let text = format_live_status(&settings, &failed);
        assert!(!text.contains("local observer active"));
        assert!(text.contains("ConfigInvalid"));
        assert!(!text.contains(" UTC"));
        assert!(text.contains("Google Drive reconciliation not confirmed"));
        let raw = LiveStatus {
            active: false,
            error: Some("prepare Google Drive folder: ConfigInvalid".into()),
            ..Default::default()
        };
        let raw_text = format_live_status(&settings, &raw);
        assert!(raw_text.contains("ConfigInvalid"));
        assert!(!raw_text.contains("local observer active"));
        assert!(!raw_text.contains("prepare Google Drive folder"));
        let successful = LiveStatus {
            active: true,
            last_upload_unix_ms: Some(951_782_400_000),
            last_full_sync_unix_ms: Some(0),
            ..Default::default()
        };
        let text = format_live_status(&settings, &successful);
        assert!(!text.contains(&format_unix_ms_local(951_782_400_000)));
        assert!(text.contains(&format_unix_ms_local(0)));
        assert!(text.contains("Latest error: None"));
    }

    #[test]
    fn live_status_presentation_reports_conflict_and_watcher_failure_safely() {
        let settings = SyncSettings { enabled: true };
        for (error, expected) in [
            ("sync conflicts require resolution", "conflict"),
            (
                "filesystem observer unavailable; using periodic reconciliation",
                "unavailable",
            ),
        ] {
            let status = LiveStatus {
                active: false,
                error: Some(error.into()),
                ..Default::default()
            };
            let text = format_live_status(&settings, &status);
            assert!(text.contains(expected));
            assert!(!text.contains("Bearer"));
        }
        let injected = LiveStatus {
            error: Some("Bearer secret-token: ConfigInvalid response body private-file".into()),
            ..Default::default()
        };
        let text = format_live_status(&settings, &injected);
        for secret in [
            "secret-token",
            "response body",
            "private-file",
            "ConfigInvalid",
        ] {
            assert!(!text.contains(secret));
        }
    }

    #[test]
    fn authorization_errors_do_not_expose_provider_or_storage_details() {
        for error in [
            auth::AuthError::OAuth("Bearer secret-token response body".into()),
            auth::AuthError::Storage("private credential file".into()),
        ] {
            let text = redacted_auth_error(&error);
            assert!(!text.contains("secret-token"));
            assert!(!text.contains("response body"));
            assert!(!text.contains("private credential"));
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("touchhle-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn conflict_times_follow_the_device_timezone_including_pre_epoch_values() {
        for timestamp_ms in [0, -1, 951_782_400_000] {
            let expected = chrono::Local
                .timestamp_millis_opt(timestamp_ms)
                .single()
                .unwrap()
                .format("%Y-%m-%d %H:%M:%S%.3f %:z")
                .to_string();
            assert_eq!(format_unix_ms_local(timestamp_ms), expected);
        }
    }

    #[test]
    fn missing_and_empty_app_directories_are_refreshable() {
        let empty_dir = temp_path("picker-empty-apps");
        std::fs::create_dir_all(&empty_dir).unwrap();
        assert!(refresh_apps(&empty_dir).unwrap().is_empty());
        std::fs::remove_dir_all(&empty_dir).unwrap();

        let missing_dir = temp_path("picker-missing-apps");
        assert!(refresh_apps(&missing_dir).unwrap().is_empty());
    }

    #[test]
    fn app_grid_model_handles_empty_transitions_and_shrinking_pages() {
        let mut model = PickerGridModel::default();
        model.replace_apps(0, 16);
        assert_eq!(model.app_count, 0);
        assert_eq!(model.current_page, 0);
        assert!(app_pages(model.app_count, 16).is_empty());

        model.replace_apps(1, 16);
        assert_eq!(model.app_count, 1);
        assert_eq!(app_pages(model.app_count, 16), vec![0..1]);

        model.replace_apps(40, 16);
        model.current_page = 2;
        model.replace_apps(1, 16);
        assert_eq!(model.current_page, 0);

        model.replace_apps(0, 16);
        assert_eq!(model.app_count, 0);
        assert_eq!(model.current_page, 0);
        assert!(app_pages(model.app_count, 16).is_empty());
    }

    #[test]
    fn only_app_events_schedule_a_quiet_picker_refresh() {
        let root = temp_path("picker-events");
        let start = Instant::now();
        let mut debounce = PickerRefreshDebounce::default();
        let app_event = LiveEvent::Changed(root.join(paths::APPS_DIR).join("game.ipa"));
        let sandbox_event = LiveEvent::Changed(root.join(paths::SANDBOX_DIR).join("save"));

        debounce.record(&root, &sandbox_event, start);
        assert!(!debounce.take_due(start + Duration::from_secs(2)));

        debounce.record(&root, &app_event, start);
        assert!(!debounce.take_due(start + Duration::from_millis(999)));
        debounce.record(&root, &app_event, start + Duration::from_millis(500));
        assert!(!debounce.take_due(start + Duration::from_millis(1_499)));
        assert!(debounce.take_due(start + Duration::from_millis(1_500)));
        assert!(!debounce.take_due(start + Duration::from_secs(3)));
    }

    #[test]
    fn finder_action_does_not_exit_the_process() {
        let source = include_str!("app_picker.rs");
        let finder_action = source
            .split("openFileManager {")
            .nth(1)
            .expect("file-manager action exists")
            .split("- (())visitWebsite")
            .next()
            .expect("file-manager action has an end");
        assert!(!finder_action.contains("std::process::exit"));
        assert!(!finder_action.contains("exiting."));
    }

    #[test]
    fn removed_picker_labels_release_their_allocated_ownership() {
        let source = include_str!("app_picker.rs");
        let refresh = source
            .split("fn refresh_picker_grid(")
            .nth(1)
            .expect("picker refresh exists")
            .split("\nfn remove_picker_label(")
            .next()
            .expect("picker refresh ends before label cleanup helper");
        assert_eq!(
            refresh.matches("remove_picker_label(env, label);").count(),
            3
        );
        assert!(!refresh.contains("release(env, icon_button)"));

        let cleanup = source
            .split("fn remove_picker_label(")
            .nth(1)
            .expect("owned-label cleanup helper exists")
            .split("\nfn make_icon_grid(")
            .next()
            .expect("owned-label cleanup helper ends before grid construction");
        let detach = cleanup
            .find("label removeFromSuperview")
            .expect("label is detached");
        let release = cleanup
            .find("release(env, label)")
            .expect("label is released");
        assert!(detach < release);
    }
}
