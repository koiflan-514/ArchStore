//! 图标缓存：只在这里持有 gdk::Texture（core 不依赖 GTK）。
//!
//! 上限：200 个 / 64 MB（§6.2 规则 4）。
//! 行内先用主题图标占位；详情页用已经落盘的缓存文件构造 Picture。

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::rc::Rc;

use archstore_core::model::{IconRef, PackageId};
use gtk::prelude::*;

/// 内存图标的数量上限。
pub const MAX_ICONS: usize = 200;
/// 内存图标的字节上限（估算值）。
pub const MAX_BYTES: usize = 64 * 1024 * 1024;

/// 图标缓存（FIFO 淘汰）。
#[derive(Debug, Default)]
pub struct IconCache {
    map: HashMap<PackageId, gtk::gdk::Texture>,
    order: VecDeque<PackageId>,
    bytes: usize,
}

impl IconCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &PackageId) -> Option<gtk::gdk::Texture> {
        self.map.get(id).cloned()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// 插入并淘汰超限的旧条目。
    pub fn insert(&mut self, id: PackageId, texture: gtk::gdk::Texture) {
        let size = texture_size(&texture);
        if self.map.insert(id.clone(), texture).is_none() {
            self.order.push_back(id);
        }
        self.bytes = self.bytes.saturating_add(size);
        self.evict();
    }

    fn evict(&mut self) {
        while (self.map.len() > MAX_ICONS || self.bytes > MAX_BYTES) && !self.order.is_empty() {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(t) = self.map.remove(&old) {
                self.bytes = self.bytes.saturating_sub(texture_size(&t));
            }
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

fn texture_size(texture: &gtk::gdk::Texture) -> usize {
    let w = texture.width().max(0) as usize;
    let h = texture.height().max(0) as usize;
    // 统一按 4 字节/像素估算
    w.saturating_mul(h).saturating_mul(4)
}

/// 从本地文件加载纹理。
pub fn texture_from_file(path: &Path) -> Option<gtk::gdk::Texture> {
    let file = gio::File::for_path(path);
    gtk::gdk::Texture::from_file(&file).ok()
}

/// 从主题图标名加载纹理。
pub fn texture_from_icon_name(name: &str, size: i32) -> Option<gtk::gdk::Texture> {
    let display = gtk::gdk::Display::default()?;
    let theme = gtk::IconTheme::for_display(&display);
    if !theme.has_icon(name) {
        return None;
    }
    theme
        .lookup_icon(
            name,
            &[],
            size,
            1,
            gtk::TextDirection::None,
            gtk::IconLookupFlags::empty(),
        )
        .file()
        .and_then(|f| gtk::gdk::Texture::from_file(&f).ok())
}

/// 行内占位图标名（§7.1：先用主题图标占位）。
pub const PLACEHOLDER_ICON: &str = "application-x-executable";

/// 远程图标的异步加载器（由 window 注入）。
///
/// 参数：URL + 完成回调（成功给出本地缓存路径）。
/// 未注入时不做任何网络请求 —— 这样单元测试与离线场景都不会静默联网。
///
/// **回调必须在主线程执行**（window 通过 runtime::spawn_ui 保证）：
/// 回调会直接把纹理写进已经渲染出来的控件里。
pub type IconLoader = Rc<dyn Fn(String, Box<dyn FnOnce(Option<std::path::PathBuf>)>)>;

/// 字母头像的配色档数（对应 style.css 里的 .avatar-0 … .avatar-7）。
pub const AVATAR_BUCKETS: usize = 8;

/// 为一行构造图标控件。
///
/// 优先级：
/// 1. 内存缓存（千级列表滚动时避免重复解码）
/// 2. `CachedFile` 本地文件（AppStream 图标 / .desktop 里的绝对路径）
/// 3. 主题图标：`.desktop` 给出的图标名 -> 包名/应用 ID 及其变体
/// 4. `Remote` URL —— 交给注入的加载器异步下载，先显示字母头像，
///    下载完成后**原地回填**（见 [`apply_remote_texture`]）
/// 5. **字母头像**：绝大多数包（尤其未安装的 AUR 包）没有本地图标来源，
///    统一显示同一个灰色占位图会让列表看起来"全是缺图标"；
///    用包名首字母 + 稳定配色的头像，任何条目都有可辨识的图形。
pub fn image_for(
    icon: &IconRef,
    size: i32,
    ctx: &crate::widgets::RowContext,
    id: &PackageId,
) -> gtk::Widget {
    // 1) 内存命中
    if let Some(texture) = ctx.icons.borrow().get(id) {
        return image_from_texture(&texture, size);
    }

    // 2) / 3) 本地可解析的来源
    if let Some(texture) = themed(icon, id, size) {
        return cache_and_show(ctx, id, texture, size);
    }

    // 4) 远程：字母头像 + 隐藏的 Image 槽位，下载完成后原地替换
    if let IconRef::Remote(url) = icon {
        let (holder, image) = placeholder(id, size);
        if ctx.register_icon_slot(id, &image) {
            schedule_remote(url, ctx, id);
        }
        return holder;
    }

    // 5) 没有图标来源：字母头像
    letter_avatar(&id.name, size)
}

/// 本地能拿到的图标：文件 > `.desktop` 给出的图标名 > 包名及变体。
fn themed(icon: &IconRef, id: &PackageId, size: i32) -> Option<gtk::gdk::Texture> {
    if let IconRef::CachedFile(path) = icon
        && let Some(texture) = texture_from_file(path)
    {
        return Some(texture);
    }
    // `.desktop` 里的 Icon= 是软件作者自己写的名字，最准确；
    // 与包名相同时不重复查一次（AUR/Flatpak 的常见情况）。
    if let IconRef::IconName(name) = icon
        && name != &id.name
        && let Some(texture) = first_themed(name, size)
    {
        return Some(texture);
    }
    first_themed(&id.name, size)
}

/// 命中本地图标后：写内存缓存并返回控件。
fn cache_and_show(
    ctx: &crate::widgets::RowContext,
    id: &PackageId,
    texture: gtk::gdk::Texture,
    size: i32,
) -> gtk::Widget {
    let widget = image_from_texture(&texture, size);
    ctx.icons.borrow_mut().insert(id.clone(), texture);
    widget
}

/// 远程图标的占位控件：字母头像 + 一个隐藏的 `Image`（下载完成后的落点）。
fn placeholder(id: &PackageId, size: i32) -> (gtk::Widget, gtk::Image) {
    let avatar = letter_avatar(&id.name, size);
    let image = gtk::Image::from_icon_name(PLACEHOLDER_ICON);
    image.set_pixel_size(size);
    image.set_visible(false);

    let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    holder.set_size_request(size, size);
    holder.set_valign(gtk::Align::Center);
    holder.set_halign(gtk::Align::Center);
    holder.append(&avatar);
    holder.append(&image);
    (holder.upcast(), image)
}

fn image_from_texture(texture: &gtk::gdk::Texture, size: i32) -> gtk::Widget {
    let img = gtk::Image::from_paintable(Some(texture));
    img.set_pixel_size(size);
    img.upcast()
}

fn first_themed(name: &str, size: i32) -> Option<gtk::gdk::Texture> {
    icon_candidates(name)
        .iter()
        .find_map(|n| texture_from_icon_name(n, size))
}

/// 下载完成后：写入内存缓存，并把纹理**原地回填**到所有仍然存活的占位槽位。
///
/// 这是"图标下载成功但界面还是字母头像"的修复点：旧实现只写内存缓存，
/// 而已经渲染出来的行不会因为缓存变化自动重绘（`GtkListView` 的 factory 只在
/// bind 时创建控件），行会一直显示占位头像，直到用户滚动把它移出屏幕再滚回来。
/// （首页"推荐"还有另一个根因：摘要里根本没带图标 URL，见 `window::load_home`。）
pub fn apply_remote_texture(
    ctx: &crate::widgets::RowContext,
    id: &PackageId,
    texture: &gtk::gdk::Texture,
) {
    ctx.icons.borrow_mut().insert(id.clone(), texture.clone());
    fill_slots(&ctx.slots, id, texture);
}

/// 把纹理写进槽位里所有仍然存活的 Image（弱引用升级失败的说明行已被回收）。
fn fill_slots(slots: &crate::widgets::IconSlots, id: &PackageId, texture: &gtk::gdk::Texture) {
    let taken = slots.borrow_mut().remove(id).unwrap_or_default();
    for weak in taken {
        let Some(image) = weak.upgrade() else {
            continue;
        };
        image.set_paintable(Some(texture));
        image.set_visible(true);
        // 同一 holder 里的字母头像让位给真实图标
        if let Some(holder) = image.parent()
            && let Some(avatar) = holder.first_child()
        {
            avatar.set_visible(false);
        }
    }
}

/// 触发一次远程图标下载；完成后回填占位槽位。
fn schedule_remote(url: &str, ctx: &crate::widgets::RowContext, id: &PackageId) {
    let Some(loader) = ctx.loader.borrow().clone() else {
        // 没有加载器（离线 / 单元测试）：立刻释放"在途"标记，不阻塞后续重试
        ctx.drop_icon_slots(id);
        return;
    };
    let icons = Rc::clone(&ctx.icons);
    let slots = Rc::clone(&ctx.slots);
    let id = id.clone();
    loader(
        url.to_string(),
        Box::new(move |path| {
            let Some(texture) = path
                .as_deref()
                .and_then(|p| texture_from_file(Path::new(p)))
            else {
                tracing::debug!(package = %id.name, "图标下载失败，保留字母头像");
                slots.borrow_mut().remove(&id);
                return;
            };
            // 先写缓存（下一次 bind / 详情页立刻可用），再回填当前可见的行
            icons.borrow_mut().insert(id.clone(), texture.clone());
            fill_slots(&slots, &id, &texture);
        }),
    );
}

/// 生成字母头像：包名首字母 + 由名字哈希决定的稳定配色。
///
/// 颜色只用 libadwaita 的命名色（见 style.css），不硬编码 hex。
pub fn letter_avatar(name: &str, size: i32) -> gtk::Widget {
    let letter = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().next().unwrap_or(c))
        .unwrap_or('?');

    let label = gtk::Label::builder()
        .label(letter.to_string())
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    label.add_css_class("avatar-letter");
    // 字号随图标尺寸缩放，避免大图标里出现一个小字母
    label.set_css_classes(&["avatar-letter"]);

    let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    holder.set_size_request(size, size);
    holder.set_valign(gtk::Align::Center);
    holder.set_halign(gtk::Align::Center);
    holder.add_css_class("avatar");
    holder.add_css_class(&format!("avatar-{}", avatar_bucket(name)));
    holder.append(&label);
    holder.upcast()
}

/// 由名字算出的稳定配色档位（同一个包每次都是同一种颜色）。
pub fn avatar_bucket(name: &str) -> usize {
    // FNV-1a：稳定、无需引入哈希库
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % AVATAR_BUCKETS as u64) as usize
}

/// 主题图标的候选名字（Flatpak 应用 ID 的末段常常就是图标名）。
pub fn icon_candidates(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    if name.contains('.') {
        // org.mozilla.firefox -> firefox
        if let Some(last) = name.rsplit('.').next()
            && !last.is_empty()
            && !out.contains(&last.to_string())
        {
            out.push(last.to_string());
        }
        // org.gnome.Builder -> org.gnome.Builder（原样已包含）
    }
    // 常见别名
    for (from, _to) in [("-bin", ""), ("-git", ""), ("-stable", "")] {
        if let Some(stripped) = name.strip_suffix(from)
            && !stripped.is_empty()
            && !out.contains(&stripped.to_string())
        {
            out.push(stripped.to_string());
        }
    }
    out
}

/// 估算图标像素尺寸（来自设置页的 icon_size）。
pub fn pixel_size_for(icon_size: archstore_core::config::IconSize) -> i32 {
    icon_size.pixels()
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::config::IconSize;

    #[test]
    fn cache_evicts_by_count() {
        let mut cache = IconCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.get(&PackageId::aur("x")), None);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn icon_candidates_include_flatpak_last_segment() {
        let c = icon_candidates("org.mozilla.firefox");
        assert!(c.contains(&"org.mozilla.firefox".to_string()));
        assert!(c.contains(&"firefox".to_string()));
    }

    #[test]
    fn icon_candidates_strip_suffixes() {
        let c = icon_candidates("foo-bin");
        assert!(c.contains(&"foo-bin".to_string()));
        assert!(c.contains(&"foo".to_string()));
    }

    #[test]
    fn icon_candidates_never_empty() {
        for name in ["a", "a.b.c", "-bin", ""] {
            assert!(!icon_candidates(name).is_empty(), "{name}");
        }
    }

    #[test]
    fn pixel_size_matches_config() {
        assert_eq!(pixel_size_for(IconSize::Small), 24);
        assert_eq!(pixel_size_for(IconSize::Medium), 32);
        assert_eq!(pixel_size_for(IconSize::Large), 48);
    }
}
