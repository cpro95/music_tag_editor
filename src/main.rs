use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use dioxus::desktop::{
    tao::event::{Event, WindowEvent},
    use_wry_event_handler,
};
use dioxus::prelude::*;
use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::tag::Tag;

const FAVICON: Asset = asset!("/assets/favicon.ico");
const MAIN_CSS: Asset = asset!("/assets/main.css");
const HEADER_SVG: Asset = asset!("/assets/header.svg");

const SUPPORTED_EXTS: [&str; 4] = ["opus", "mp3", "ogg", "flac"];

#[derive(Clone, Default, PartialEq)]
struct AudioMetadata {
    file_name: String,
    file_path: PathBuf,
    format_label: String,
    title: String,
    artist: String,
    album_artist: String,
    album: String,
    date: String,
}

#[derive(Clone, PartialEq)]
struct CoverPreview {
    folder_name: String,
    file_name: String,
    data_uri: String,
}

#[derive(Clone, PartialEq)]
enum TreeEntry {
    Dir {
        path: PathBuf,
        name: String,
        depth: usize,
        file_count: usize,
        collapsed: bool,
        has_cover: bool,
    },
    File {
        path: PathBuf,
        name: String,
        ext: String,
        depth: usize,
    },
}

/// 백그라운드 ffplay 프로세스 관리 (Drop 시 자동 종료)
struct ActivePlayback {
    path: PathBuf,
    child: Child,
}

impl Drop for ActivePlayback {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 앱 종료 시에도 확실히 정리하기 위해 전역으로 프로세스 보관
static ACTIVE_PLAYBACK: Mutex<Option<ActivePlayback>> = Mutex::new(None);

fn stop_global_playback() {
    if let Ok(mut guard) = ACTIVE_PLAYBACK.lock() {
        *guard = None;
    }
}

struct PlaybackCleanupGuard;

impl Drop for PlaybackCleanupGuard {
    fn drop(&mut self) {
        stop_global_playback();
    }
}

fn main() {
    dioxus::launch(App);
    stop_global_playback();
}

#[component]
fn App() -> Element {
    rsx! {
        document::Link { rel: "icon", href: FAVICON }
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        TagEditor {}
    }
}

#[component]
pub fn TagEditor() -> Element {
    // 컴포넌트 언마운트 시 ffplay 정리
    use_hook(|| std::rc::Rc::new(PlaybackCleanupGuard));

    // 창 닫기(X) 또는 앱 종료(Cmd+Q 등) 시 즉시 ffplay 강제 종료
    use_wry_event_handler(move |event, _target| match event {
        Event::WindowEvent {
            event: WindowEvent::CloseRequested | WindowEvent::Destroyed,
            ..
        }
        | Event::LoopDestroyed => {
            stop_global_playback();
        }
        _ => {}
    });

    // macOS WKWebView에서 Space/방향키 첫 입력 시 시스템 경고음(NSBeep) 방지
    use_effect(move || {
        document::eval(
            r#"
            if (!window.__musigKeyFix) {
                window.__musigKeyFix = true;
                const preventNavBeep = (e) => {
                    const list = document.querySelector('.file-list');
                    if (!list) return;
                    const active = document.activeElement;
                    if (active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA')) return;
                    if (active === list || list.contains(active)) {
                        if (e.key === ' ' || e.key === 'ArrowUp' || e.key === 'ArrowDown') {
                            e.preventDefault();
                        }
                    }
                };
                window.addEventListener('keydown', preventNavBeep, true);
                window.addEventListener('keypress', preventNavBeep, true);
            }
            "#,
        );
    });

    // 왼쪽 패널: 루트 폴더, 접힌 폴더 목록, 선택된 파일 경로, 커버 미리보기
    let mut folder_path = use_signal(|| Option::<PathBuf>::None);
    let mut collapsed_dirs = use_signal(HashSet::<PathBuf>::new);
    let mut selected_path = use_signal(|| Option::<PathBuf>::None);
    let mut tree_entries = use_signal(Vec::<TreeEntry>::new);
    let mut total_file_count = use_signal(|| 0usize);
    let mut cover_preview = use_signal(|| Option::<CoverPreview>::None);

    // UI 표시용 현재 재생 중인 곡 정보: (파일 경로, 파일명)
    let mut playing_info = use_signal(|| Option::<(PathBuf, String)>::None);

    // 오른쪽 상단: 현재 선택된 파일에서 읽어온 기존 메타데이터
    let mut current_meta = use_signal(|| Option::<AudioMetadata>::None);

    // 오른쪽 하단: 스포티파이 붙여넣기 및 편집 입력 필드
    let mut raw_metadata = use_signal(String::new);
    let mut title = use_signal(String::new);
    let mut artist = use_signal(String::new);
    let mut album_artist = use_signal(String::new);
    let mut album = use_signal(String::new);
    let mut date = use_signal(String::new);

    // 상태 알림 메시지: (성공 여부, 메시지)
    let mut status_msg = use_signal(|| Option::<(bool, String)>::None);

    // 재생 정지 공통 헬퍼
    let mut stop_playback = move || {
        stop_global_playback();
        playing_info.set(None);
    };

    // 트리 목록 다시 빌드
    let mut refresh_tree = move |root: &Path| {
        let collapsed = collapsed_dirs.read().clone();
        let (count, entries) = build_tree_entries(root, 0, &collapsed);
        total_file_count.set(count);
        tree_entries.set(entries);
    };

    // 특정 경로의 파일을 선택하고 현재 메타데이터 + 해당 폴더 cover 읽기
    let mut select_file_path = move |path: PathBuf| {
        selected_path.set(Some(path.clone()));
        cover_preview.set(path.parent().and_then(load_cover_preview));

        match read_file_metadata(&path) {
            Ok(meta) => {
                current_meta.set(Some(meta));
                status_msg.set(None);
            }
            Err(err) => {
                current_meta.set(None);
                status_msg.set(Some((false, err)));
            }
        }
        document::eval("document.querySelector('.file-list')?.focus()");
    };

    // 스페이스바로 선택된 곡 재생 / 정지 토글
    let mut toggle_play = move |target: PathBuf| {
        // 이미 자연 종료된 프로세스가 남아있다면 정리
        let already_finished = {
            if let Ok(mut guard) = ACTIVE_PLAYBACK.lock() {
                if let Some(pb) = guard.as_mut() {
                    matches!(pb.child.try_wait(), Ok(Some(_)))
                } else {
                    false
                }
            } else {
                false
            }
        };
        if already_finished {
            stop_playback();
        }

        // 같은 곡이 재생 중이면 정지
        let is_same_playing = ACTIVE_PLAYBACK
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|pb| pb.path == target))
            .unwrap_or(false);
        if is_same_playing {
            stop_playback();
            return;
        }

        // 다른 곡이 재생 중이었다면 먼저 정지
        stop_playback();

        match spawn_ffplay(&target) {
            Ok(child) => {
                let file_name = target
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if let Ok(mut guard) = ACTIVE_PLAYBACK.lock() {
                    *guard = Some(ActivePlayback {
                        path: target.clone(),
                        child,
                    });
                }
                playing_info.set(Some((target, file_name)));
            }
            Err(err) => {
                status_msg.set(Some((false, err)));
            }
        }
    };

    // 왼쪽: 폴더 선택 버튼
    let on_pick_folder = move |_| {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            stop_playback();
            collapsed_dirs.set(HashSet::new());
            folder_path.set(Some(dir.clone()));

            let (count, entries) = build_tree_entries(&dir, 0, &HashSet::new());
            total_file_count.set(count);
            tree_entries.set(entries.clone());

            let first_file = entries.iter().find_map(|e| match e {
                TreeEntry::File { path, .. } => Some(path.clone()),
                _ => None,
            });

            if let Some(first) = first_file {
                select_file_path(first);
            } else {
                selected_path.set(None);
                current_meta.set(None);
                cover_preview.set(load_cover_preview(&dir));
                status_msg.set(Some((
                    false,
                    "선택한 폴더(하위 폴더 포함)에 .opus, .mp3, .ogg, .flac 파일이 없습니다."
                        .to_string(),
                )));
            }
        }
    };

    // 왼쪽: 키보드 위/아래 방향키 이동 및 Space 재생/정지
    let on_list_keydown = move |evt: KeyboardEvent| {
        match evt.key() {
            Key::Character(ref s) if s == " " => {
                evt.prevent_default();
                if let Some(path) = selected_path.read().clone() {
                    toggle_play(path);
                }
                return;
            }
            Key::ArrowDown | Key::ArrowUp => {}
            _ => return,
        }

        let visible_files: Vec<PathBuf> = tree_entries
            .read()
            .iter()
            .filter_map(|e| match e {
                TreeEntry::File { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();

        if visible_files.is_empty() {
            return;
        }

        let current_idx = selected_path
            .read()
            .as_ref()
            .and_then(|sel| visible_files.iter().position(|p| p == sel));

        let next_idx = match evt.key() {
            Key::ArrowDown => {
                evt.prevent_default();
                match current_idx {
                    Some(i) => (i + 1).min(visible_files.len() - 1),
                    None => 0,
                }
            }
            Key::ArrowUp => {
                evt.prevent_default();
                match current_idx {
                    Some(i) => i.saturating_sub(1),
                    None => 0,
                }
            }
            _ => return,
        };

        select_file_path(visible_files[next_idx].clone());
        document::eval(
            "document.querySelector('.file-item.active')?.scrollIntoView({ block: 'nearest' })",
        );
    };

    // 오른쪽: 입력 칸 초기화 (왼쪽 폴더 목록은 유지)
    let on_reset_inputs = move |_| {
        raw_metadata.set(String::new());
        title.set(String::new());
        artist.set(String::new());
        album_artist.set(String::new());
        album.set(String::new());
        date.set(String::new());
        status_msg.set(None);
    };

    // 현재 파일의 메타데이터를 아래 편집 칸으로 복사
    let on_load_current_to_form = move |_| {
        if let Some(meta) = current_meta.read().as_ref() {
            title.set(meta.title.clone());
            artist.set(meta.artist.clone());
            album_artist.set(meta.album_artist.clone());
            album.set(meta.album.clone());
            date.set(meta.date.clone());
            status_msg.set(Some((
                true,
                "현재 파일의 태그를 아래 입력 칸으로 불러왔습니다.".to_string(),
            )));
        }
    };

    // 스포티파이 메타정보 분석하기
    let on_parse = move |_| {
        let text = raw_metadata.read();
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != "•" && *line != "·")
            .collect();

        if lines.is_empty() {
            status_msg.set(Some((
                false,
                "분석할 메타정보를 먼저 붙여넣어 주세요.".to_string(),
            )));
            return;
        }

        title.set(lines.first().copied().unwrap_or("").to_string());
        artist.set(lines.get(1).copied().unwrap_or("").to_string());
        album_artist.set(lines.get(2).copied().unwrap_or("").to_string());
        album.set(lines.get(3).copied().unwrap_or("").to_string());
        date.set(lines.get(4).copied().unwrap_or("").to_string());

        status_msg.set(Some((
            true,
            format!(
                "{}개 항목을 분석해 아래 입력 칸에 반영했습니다.",
                lines.len().min(5)
            ),
        )));
    };

    // Lofty로 실제 오디오 파일 태그 수정 및 파일명 변경
    let on_apply_tags = move |_| {
        let Some(meta) = current_meta.read().clone() else {
            status_msg.set(Some((
                false,
                "먼저 왼쪽 목록에서 파일을 선택해 주세요.".to_string(),
            )));
            return;
        };

        let path = meta.file_path;
        if !path.exists() {
            status_msg.set(Some((
                false,
                format!("파일을 찾을 수 없습니다: {}", path.display()),
            )));
            return;
        }

        // 현재 재생 중인 파일이면 수정 전 정지
        let is_playing_this = playing_info
            .read()
            .as_ref()
            .is_some_and(|(p, _)| p == &path);
        if is_playing_this {
            stop_playback();
        }

        match apply_tags_and_rename(
            &path,
            &title.read(),
            &artist.read(),
            &album_artist.read(),
            &album.read(),
            &date.read(),
        ) {
            Ok((final_path, renamed, warn_msg)) => {
                selected_path.set(Some(final_path.clone()));
                if let Some(dir) = folder_path.read().clone() {
                    refresh_tree(&dir);
                }

                if let Ok(updated_meta) = read_file_metadata(&final_path) {
                    current_meta.set(Some(updated_meta));
                }

                let msg = if let Some(warn) = warn_msg {
                    warn
                } else if renamed {
                    format!(
                        "태그 저장 및 파일명 변경 완료!\n→ {}",
                        final_path.file_name().and_then(|n| n.to_str()).unwrap_or("")
                    )
                } else {
                    "태그 저장 완료!".to_string()
                };
                status_msg.set(Some((true, msg)));
            }
            Err(err) => {
                status_msg.set(Some((false, format!("태그 수정 실패: {err}"))));
            }
        }
    };

    // 폴더 자동분류하기: {루트}/{아티스트}/{앨범}/{파일명} 으로 이동
    let on_organize_folder = move |_| {
        let Some(root) = folder_path.read().clone() else {
            return;
        };
        let Some(meta) = current_meta.read().clone() else {
            status_msg.set(Some((
                false,
                "먼저 왼쪽 목록에서 파일을 선택해 주세요.".to_string(),
            )));
            return;
        };

        let Some(target_path) = compute_target_organize_path(&root, &meta) else {
            status_msg.set(Some((
                false,
                "아티스트(또는 앨범 아티스트)와 앨범 태그가 모두 있어야 폴더 분류가 가능합니다."
                    .to_string(),
            )));
            return;
        };

        if meta.file_path == target_path {
            return;
        }

        if target_path.exists() {
            status_msg.set(Some((
                false,
                format!(
                    "대상 폴더에 같은 이름의 파일이 이미 존재합니다:\n→ {}",
                    target_path.display()
                ),
            )));
            return;
        }

        // 현재 재생 중인 파일이면 이동 전 정지
        let is_playing_this = playing_info
            .read()
            .as_ref()
            .is_some_and(|(p, _)| p == &meta.file_path);
        if is_playing_this {
            stop_playback();
        }

        if let Some(target_dir) = target_path.parent() {
            if let Err(err) = std::fs::create_dir_all(target_dir) {
                status_msg.set(Some((false, format!("폴더 생성 실패: {err}"))));
                return;
            }
        }

        if let Err(err) = std::fs::rename(&meta.file_path, &target_path) {
            status_msg.set(Some((false, format!("파일 이동 실패: {err}"))));
            return;
        }

        {
            let mut collapsed = collapsed_dirs.write();
            if let Some(album_dir) = target_path.parent() {
                collapsed.remove(album_dir);
                if let Some(artist_dir) = album_dir.parent() {
                    collapsed.remove(artist_dir);
                }
            }
        }

        selected_path.set(Some(target_path.clone()));
        refresh_tree(&root);
        cover_preview.set(target_path.parent().and_then(load_cover_preview));

        if let Ok(updated_meta) = read_file_metadata(&target_path) {
            current_meta.set(Some(updated_meta));
        }

        let rel_display = target_path
            .strip_prefix(&root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| target_path.display().to_string());

        status_msg.set(Some((true, format!("폴더 자동분류 완료!\n→ {rel_display}"))));
        document::eval(
            "document.querySelector('.file-item.active')?.scrollIntoView({ block: 'nearest' })",
        );
    };

    let entries_snapshot = tree_entries.read().clone();
    let current_selected = selected_path.read().clone();
    let current_root = folder_path.read().clone();
    let meta_snapshot = current_meta.read().clone();
    let cover_snapshot = cover_preview.read().clone();
    let playing_snapshot = playing_info.read().clone();

    let is_already_organized = match (&current_root, &meta_snapshot) {
        (Some(root), Some(meta)) => compute_target_organize_path(root, meta)
            .is_some_and(|target| target == meta.file_path),
        _ => false,
    };

    rsx! {
        div { class: "app-shell",
            // ================= 왼쪽 패널: 폴더 선택 & 트리 & 플레이어 & 커버 =================
            aside { class: "left-pane",
                div { class: "left-header",
                    button { class: "btn-folder", onclick: on_pick_folder, "📁 폴더 선택" }

                    if let Some(dir) = current_root.as_ref() {
                        div { class: "folder-info",
                            p {
                                class: "folder-path",
                                title: "{dir.display()}",
                                "{dir.display()}"
                            }
                            span { class: "file-count", "총 {total_file_count}개 파일" }
                        }
                    }
                }

                div {
                    class: "file-list",
                    tabindex: "0",
                    onkeydown: on_list_keydown,

                    if entries_snapshot.is_empty() {
                        div { class: "list-empty",
                            p { "폴더를 선택하면 폴더와 오디오 파일이 표시됩니다." }
                            span { "(.opus, .mp3, .ogg, .flac)" }
                        }
                    } else {
                        for entry in entries_snapshot {
                            match entry {
                                TreeEntry::Dir { path, name, depth, file_count, collapsed, has_cover } => {
                                    let indent_px = 8 + depth * 14;
                                    let dir_for_toggle = path.clone();
                                    rsx! {
                                        div {
                                            key: "dir-{path.display()}",
                                            class: "dir-item",
                                            style: "padding-left: {indent_px}px;",
                                            onclick: move |_| {
                                                {
                                                    let mut set = collapsed_dirs.write();
                                                    if set.contains(&dir_for_toggle) {
                                                        set.remove(&dir_for_toggle);
                                                    } else {
                                                        set.insert(dir_for_toggle.clone());
                                                    }
                                                }
                                                if let Some(cover) = load_cover_preview(&dir_for_toggle) {
                                                    cover_preview.set(Some(cover));
                                                }
                                                if let Some(root) = folder_path.read().clone() {
                                                    refresh_tree(&root);
                                                }
                                            },
                                            span { class: "dir-arrow",
                                                if collapsed {
                                                    "▸"
                                                } else {
                                                    "▾"
                                                }
                                            }
                                            span { class: "dir-icon", "📂" }
                                            span { class: "dir-name", title: "{name}", "{name}" }
                                            if has_cover {
                                                span { class: "cover-badge", title: "앨범 커버 있음", "🖼️" }
                                            }
                                            span { class: "dir-count", "{file_count}" }
                                        }
                                    }
                                }
                                TreeEntry::File { path, name, ext, depth } => {
                                    let indent_px = 10 + depth * 14;
                                    let is_active = current_selected.as_ref() == Some(&path);
                                    let is_playing_row = playing_snapshot
                                        .as_ref()
                                        .is_some_and(|(p, _)| p == &path);
                                    let file_for_click = path.clone();
                                    rsx! {
                                        div {
                                            key: "file-{path.display()}",
                                            class: if is_active { "file-item active" } else { "file-item" },
                                            style: "padding-left: {indent_px}px;",
                                            onclick: move |_| {
                                                select_file_path(file_for_click.clone());
                                            },
                                            span { class: "ext-badge", "{ext}" }
                                            if is_playing_row {
                                                span { class: "playing-icon", "🔊" }
                                            }
                                            span { class: "item-name", title: "{name}", "{name}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // 미니 재생 상태 바
                if let Some((_, playing_name)) = playing_snapshot {
                    div { class: "player-bar playing",
                        span { class: "player-dot", "▶" }
                        span { class: "player-title", title: "{playing_name}", "{playing_name}" }
                        button {
                            class: "btn-stop",
                            onclick: move |_| stop_playback(),
                            "⏹ 정지"
                        }
                    }
                } else {
                    div { class: "player-bar idle",
                        span { "Space: 선택된 곡 재생 / 정지" }
                    }
                }

                // 왼쪽 하단: 선택된 앨범 폴더의 cover 이미지 뷰어
                div { class: "cover-panel",
                    if let Some(cover) = cover_snapshot.as_ref() {
                        div { class: "cover-art-wrap",
                            img {
                                class: "cover-img",
                                src: "{cover.data_uri}",
                                alt: "Album Cover",
                            }
                        }
                        div { class: "cover-caption",
                            span {
                                class: "cover-folder",
                                title: "{cover.folder_name}",
                                "{cover.folder_name}"
                            }
                            span { class: "cover-file", "{cover.file_name}" }
                        }
                    } else {
                        div { class: "cover-empty",
                            span { class: "cover-empty-icon", "💿" }
                            p { "커버 이미지 없음" }
                        }
                    }
                }
            }

            // ================= 오른쪽 패널: 현재 메타정보 & 편집기 =================
            main { class: "right-pane",
                header { class: "header-row",
                    div { class: "header-brand",
                        img {
                            src: HEADER_SVG,
                            class: "header-logo",
                            alt: "Musig Tag Editor Logo",
                        }
                        div { class: "header",
                            h1 { "Musig Tag Editor" }
                            p { "OPUS / MP3 / OGG / FLAC 메타데이터 편집기" }
                        }
                    }
                    button {
                        class: "btn-reset",
                        title: "입력 칸만 초기화합니다",
                        onclick: on_reset_inputs,
                        "↺ 초기화"
                    }
                }

                // 1. 현재 선택된 파일의 메타데이터 정보 카드
                div { class: "meta-card",
                    if let Some(meta) = meta_snapshot.as_ref() {
                        div { class: "meta-card-header",
                            div { class: "meta-title-wrap",
                                span { class: "ext-badge", "{meta.format_label}" }
                                span { class: "meta-filename", "{meta.file_name}" }
                            }
                            button {
                                class: "btn-mini",
                                onclick: on_load_current_to_form,
                                "입력칸으로 불러오기"
                            }
                        }
                        div { class: "meta-grid",
                            div { class: "meta-row meta-row-full",
                                span { class: "meta-key", "Title" }
                                span { class: "meta-val", "{display_or_empty(&meta.title)}" }
                            }
                            div { class: "meta-row",
                                span { class: "meta-key", "Artist" }
                                span { class: "meta-val", "{display_or_empty(&meta.artist)}" }
                            }
                            div { class: "meta-row",
                                span { class: "meta-key", "Album Artist" }
                                span { class: "meta-val", "{display_or_empty(&meta.album_artist)}" }
                            }
                            div { class: "meta-row",
                                span { class: "meta-key", "Album" }
                                span { class: "meta-val", "{display_or_empty(&meta.album)}" }
                            }
                            div { class: "meta-row",
                                span { class: "meta-key", "Year (date)" }
                                span { class: "meta-val", "{display_or_empty(&meta.date)}" }
                            }
                        }
                    } else {
                        div { class: "meta-empty",
                            "왼쪽 목록에서 파일을 선택하면 현재 메타데이터가 여기에 표시됩니다."
                        }
                    }
                }

                // 2. 스포티파이 메타정보 붙여넣기 & 분석하기 버튼
                div { class: "paste-section",
                    label { r#for: "raw-metadata", "Spotify 메타정보 붙여넣기" }
                    textarea {
                        id: "raw-metadata",
                        rows: "5",
                        placeholder: "타이틀\n아티스트\n앨범아티스트\n•\n앨범명\n•\n년도",
                        value: "{raw_metadata}",
                        oninput: move |evt| raw_metadata.set(evt.value()),
                    }
                    button { class: "btn-secondary", onclick: on_parse, "메타정보 분석하기" }
                }

                // 3. 개별 메타데이터 입력 폼
                div { class: "form-grid",
                    div { class: "field field-full",
                        label { r#for: "title", "Title" }
                        input {
                            id: "title",
                            r#type: "text",
                            value: "{title}",
                            oninput: move |evt| title.set(evt.value()),
                        }
                    }

                    div { class: "field",
                        label { r#for: "artist", "Artist" }
                        input {
                            id: "artist",
                            r#type: "text",
                            value: "{artist}",
                            oninput: move |evt| artist.set(evt.value()),
                        }
                    }

                    div { class: "field",
                        label { r#for: "album_artist", "Album Artist" }
                        input {
                            id: "album_artist",
                            r#type: "text",
                            value: "{album_artist}",
                            oninput: move |evt| album_artist.set(evt.value()),
                        }
                    }

                    div { class: "field",
                        label { r#for: "album", "Album" }
                        input {
                            id: "album",
                            r#type: "text",
                            value: "{album}",
                            oninput: move |evt| album.set(evt.value()),
                        }
                    }

                    div { class: "field",
                        label { r#for: "date", "Year (date)" }
                        input {
                            id: "date",
                            r#type: "text",
                            value: "{date}",
                            oninput: move |evt| date.set(evt.value()),
                        }
                    }
                }

                // 4. 실행 버튼들 (태그 저장 및 파일명 변경 + 폴더 자동분류하기)
                div { class: "actions",
                    button { class: "btn-primary", onclick: on_apply_tags,
                        "태그 저장 및 파일명 변경"
                    }
                    button {
                        class: if is_already_organized || meta_snapshot.is_none() { "btn-organize disabled" } else { "btn-organize" },
                        disabled: is_already_organized || meta_snapshot.is_none(),
                        onclick: on_organize_folder,
                        if is_already_organized {
                            "✓ 이미 올바른 폴더에 분류됨"
                        } else {
                            "📂 폴더 자동분류하기"
                        }
                    }
                }

                // 상태 알림
                if let Some((ok, msg)) = status_msg.read().as_ref() {
                    div {
                        class: if *ok { "status success" } else { "status error" },
                        style: "white-space: pre-line;",
                        "{msg}"
                    }
                }
            }
        }
    }
}

fn find_ffplay_command() -> &'static str {
    for candidate in [
        "/opt/homebrew/bin/ffplay",
        "/usr/local/bin/ffplay",
        "/opt/local/bin/ffplay",
    ] {
        if Path::new(candidate).is_file() {
            return candidate;
        }
    }
    "ffplay"
}

fn spawn_ffplay(path: &Path) -> Result<Child, String> {
    let bin = find_ffplay_command();
    let mut cmd = Command::new(bin);
    cmd.args(["-nodisp", "-vn", "-autoexit", "-loglevel", "quiet"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    cmd.spawn()
        .map_err(|e| format!("ffplay 실행 실패 ({e}). ffmpeg/ffplay가 설치되어 있는지 확인해 주세요."))
}

fn display_or_empty(s: &str) -> &str {
    if s.trim().is_empty() {
        "(비어 있음)"
    } else {
        s
    }
}

/// 폴더 내 cover 이미지(jpg, jpeg, webp, png) 파일 탐색
fn find_cover_in_dir(dir: &Path) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };

    let mut found_jpeg = None;
    let mut found_webp = None;
    let mut found_png = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        match name.to_lowercase().as_str() {
            "cover.jpg" => return Some(path),
            "cover.jpeg" if found_jpeg.is_none() => found_jpeg = Some(path),
            "cover.webp" if found_webp.is_none() => found_webp = Some(path),
            "cover.png" if found_png.is_none() => found_png = Some(path),
            _ => {}
        }
    }

    found_jpeg.or(found_webp).or(found_png)
}

/// cover 이미지(jpg, jpeg, webp, png)를 읽어 Data URI로 변환
fn load_cover_preview(dir: &Path) -> Option<CoverPreview> {
    let cover_path = find_cover_in_dir(dir)?;
    let bytes = std::fs::read(&cover_path).ok()?;

    let ext = cover_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    let mime = match ext.as_str() {
        "png" => "image/png",
        "webp" => "image/webp",
        _ => "image/jpeg",
    };

    let data_uri = format!("data:{mime};base64,{}", encode_base64(&bytes));
    let folder_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let file_name = cover_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("cover")
        .to_string();

    Some(CoverPreview {
        folder_name,
        file_name,
        data_uri,
    })
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };

        out.push(TABLE[b0 >> 2] as char);
        out.push(TABLE[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[b2 & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 폴더와 하위 폴더 내 오디오 파일을 재귀적으로 스캔하여 트리 엔트리 생성
fn build_tree_entries(
    dir: &Path,
    depth: usize,
    collapsed_dirs: &HashSet<PathBuf>,
) -> (usize, Vec<TreeEntry>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return (0, Vec::new());
    };

    let mut subdirs = Vec::new();
    let mut files = Vec::new();

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.is_file() {
            let is_supported = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|ext| SUPPORTED_EXTS.contains(&ext.to_lowercase().as_str()))
                .unwrap_or(false);
            if is_supported {
                files.push(path);
            }
        }
    }

    let sort_by_name = |a: &PathBuf, b: &PathBuf| {
        let na = a
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();
        let nb = b
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();
        na.cmp(&nb)
    };

    subdirs.sort_by(sort_by_name);
    files.sort_by(sort_by_name);

    let mut total_count = files.len();
    let mut entries = Vec::new();

    for subdir in subdirs {
        let (sub_count, sub_entries) = build_tree_entries(&subdir, depth + 1, collapsed_dirs);
        if sub_count > 0 {
            total_count += sub_count;
            let collapsed = collapsed_dirs.contains(&subdir);
            let has_cover = find_cover_in_dir(&subdir).is_some();
            let name = subdir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            entries.push(TreeEntry::Dir {
                path: subdir,
                name,
                depth,
                file_count: sub_count,
                collapsed,
                has_cover,
            });

            if !collapsed {
                entries.extend(sub_entries);
            }
        }
    }

    for file in files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_uppercase();

        entries.push(TreeEntry::File {
            path: file,
            name,
            ext,
            depth,
        });
    }

    (total_count, entries)
}

/// Lofty로 파일의 현재 메타데이터 읽기
fn read_file_metadata(path: &Path) -> Result<AudioMetadata, String> {
    let tagged_file =
        lofty::read_from_path(path).map_err(|e| format!("메타데이터 읽기 실패: {e}"))?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let format_label = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_uppercase();

    let mut meta = AudioMetadata {
        file_name,
        file_path: path.to_path_buf(),
        format_label,
        ..Default::default()
    };

    if let Some(tag) = tag {
        meta.title = tag.title().map(|s| s.to_string()).unwrap_or_default();
        meta.artist = tag.artist().map(|s| s.to_string()).unwrap_or_default();
        meta.album_artist = tag
            .get_string(ItemKey::AlbumArtist)
            .unwrap_or("")
            .to_string();
        meta.album = tag.album().map(|s| s.to_string()).unwrap_or_default();
        meta.date = tag
            .get_string(ItemKey::RecordingDate)
            .or_else(|| tag.get_string(ItemKey::Year))
            .unwrap_or("")
            .to_string();
    }

    Ok(meta)
}

/// OS 파일명에서 금지된 문자 치환
fn sanitize_filename(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

/// 폴더명용 정리 (Windows 등에서 끝의 공백/마침표 문제 방지)
fn sanitize_folder_name(name: &str) -> String {
    sanitize_filename(name)
        .trim_matches(|c: char| c.is_whitespace() || c == '.')
        .to_string()
}

/// 목표 분류 경로 계산: {루트}/{AlbumArtist 또는 Artist}/{Album}/{파일명}
fn compute_target_organize_path(root: &Path, meta: &AudioMetadata) -> Option<PathBuf> {
    let artist_raw = if !meta.album_artist.trim().is_empty() {
        meta.album_artist.trim()
    } else {
        meta.artist.trim()
    };
    let album_raw = meta.album.trim();

    if artist_raw.is_empty() || album_raw.is_empty() {
        return None;
    }

    let safe_artist = sanitize_folder_name(artist_raw);
    let safe_album = sanitize_folder_name(album_raw);
    if safe_artist.is_empty() || safe_album.is_empty() {
        return None;
    }

    Some(root.join(safe_artist).join(safe_album).join(&meta.file_name))
}

/// Lofty를 이용해 태그를 파일에 직접 기록하고, Title 기반으로 파일명 변경
fn apply_tags_and_rename(
    path: &Path,
    title: &str,
    artist: &str,
    album_artist: &str,
    album: &str,
    date: &str,
) -> Result<(PathBuf, bool, Option<String>), String> {
    let mut tagged_file =
        lofty::read_from_path(path).map_err(|e| format!("파일 읽기 오류: {e}"))?;

    let tag = match tagged_file.primary_tag_mut() {
        Some(existing) => existing,
        None => {
            let tag_type = tagged_file.primary_tag_type();
            tagged_file.insert_tag(Tag::new(tag_type));
            tagged_file
                .primary_tag_mut()
                .ok_or_else(|| "태그를 생성할 수 없습니다.".to_string())?
        }
    };

    if !title.trim().is_empty() {
        tag.set_title(title.trim().to_string());
    }
    if !artist.trim().is_empty() {
        tag.set_artist(artist.trim().to_string());
    }
    if !album_artist.trim().is_empty() {
        tag.insert_text(ItemKey::AlbumArtist, album_artist.trim().to_string());
    }
    if !album.trim().is_empty() {
        tag.set_album(album.trim().to_string());
    }
    if !date.trim().is_empty() {
        tag.insert_text(ItemKey::RecordingDate, date.trim().to_string());
    }

    tagged_file
        .save_to_path(path, WriteOptions::default())
        .map_err(|e| format!("태그 저장 오류: {e}"))?;

    // Title이 있으면 같은 폴더 내에서 "{Title}.{ext}"로 파일명 변경
    let safe_title = sanitize_filename(title);
    if !safe_title.is_empty() {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("opus");
        let new_path = path.with_file_name(format!("{safe_title}.{ext}"));

        if new_path != path {
            if new_path.exists() {
                return Ok((
                    path.to_path_buf(),
                    false,
                    Some(format!(
                        "태그는 저장되었으나 같은 이름의 파일이 이미 있어 파일명은 유지했습니다.\n→ {}",
                        path.display()
                    )),
                ));
            }
            std::fs::rename(path, &new_path)
                .map_err(|e| format!("태그는 저장되었으나 파일명 변경 실패: {e}"))?;
            return Ok((new_path, true, None));
        }
    }

    Ok((path.to_path_buf(), false, None))
}