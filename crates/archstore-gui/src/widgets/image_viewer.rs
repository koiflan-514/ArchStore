//! 演示图片（截图）查看器：在详情页单击某张截图后，以接近全屏的窗口查看它。
//!
//! 设计取舍：
//! - 用独立的 `GtkWindow`（transient，非模态）而不是对话框：截图需要尽可能大的画面，
//!   而且用户应当能同时对照详情页；非模态才不会挡着后面。
//! - 图片一律从**已经下载到本地缓存**的文件读取（`GtkPicture` 没有 URL 加载 API），
//!   尚未下载完的槽位显示占位提示，绝不在查看器里再发一次网络请求。
//! - 左右方向键 / 两个按钮切换同一条目下的多张截图，`Esc` 关闭。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::prelude::*;

use crate::ui;

/// 在 `0..count` 范围内按 `delta` 移动索引；越界时停在边界（而不是回绕）。
///
/// 回绕会让"下一张"在最后一张时突然跳回第一张，用户很难判断自己看到的是第几张。
fn step(index: usize, count: usize, delta: i64) -> usize {
    if count == 0 {
        return 0;
    }
    let next = index as i64 + delta;
    next.clamp(0, count as i64 - 1) as usize
}

/// 打开截图查看器。
///
/// - `urls` 与 `files` 一一对应，两张列表的较短者决定张数（防止调用方传错长度）；
/// - `files[i]` 是第 i 张截图已下载到的本地文件，`None` 表示还没下载完；
/// - `index`：单击的那一张。
pub fn show(
    parent: &impl IsA<gtk::Window>,
    urls: &[String],
    files: &[Option<PathBuf>],
    index: usize,
) {
    let count = urls.len().min(files.len());
    if count == 0 {
        return;
    }

    let window = gtk::Window::builder()
        .title(ui::t("查看演示图片"))
        .transient_for(parent)
        .modal(false)
        .default_width(1080)
        .default_height(720)
        .build();
    if let Some(app) = parent.application() {
        window.set_application(Some(&app));
    }

    let title = gtk::Label::builder().css_classes(["title"]).build();
    let prev = gtk::Button::from_icon_name("go-previous-symbolic");
    prev.set_tooltip_text(Some(&ui::t("上一张（←）")));
    let next = gtk::Button::from_icon_name("go-next-symbolic");
    next.set_tooltip_text(Some(&ui::t("下一张（→）")));

    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_start(&prev);
    header.pack_end(&next);

    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["screenshot-full"])
        .build();

    // 还没下载完的槽位：给出占位提示而不是空白窗口
    let missing = ui::placeholder(
        "image-loading-symbolic",
        &ui::t("图片尚未下载完成"),
        &ui::t("回到详情页稍候，图片下载完成后即可查看。"),
    );

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    stack.add_named(&picture, Some("image"));
    stack.add_named(&missing, Some("missing"));

    let scroller = gtk::ScrolledWindow::builder()
        .child(&stack)
        .vexpand(true)
        .hexpand(true)
        .build();
    scroller.add_css_class("screenshot-viewer");

    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&header);
    body.append(&scroller);
    window.set_child(Some(&body));

    let files: Rc<Vec<Option<PathBuf>>> = Rc::new(files[..count].to_vec());

    // 渲染第 index 张
    let render: Rc<dyn Fn(usize)> = {
        let picture = picture.clone();
        let stack = stack.clone();
        let title = title.clone();
        let prev = prev.clone();
        let next = next.clone();
        let files = Rc::clone(&files);
        Rc::new(move |index: usize| {
            let index = step(index, count, 0);
            match files.get(index).and_then(|f| f.clone()) {
                Some(path) => {
                    picture.set_file(Some(&gio::File::for_path(&path)));
                    stack.set_visible_child_name("image");
                }
                None => {
                    picture.set_file(None::<&gio::File>);
                    stack.set_visible_child_name("missing");
                }
            }
            title.set_label(&format!("{} {} / {}", ui::t("演示图片"), index + 1, count));
            // 只有一张、或已经在两端时按钮置灰，避免"点了没反应"
            prev.set_sensitive(index > 0);
            next.set_sensitive(index + 1 < count);
        })
    };

    // 切到相邻的一张（按钮与方向键共用同一条路径）
    let current = Rc::new(Cell::new(step(index, count, 0)));
    let go: Rc<dyn Fn(i64)> = {
        let render = Rc::clone(&render);
        let current = Rc::clone(&current);
        Rc::new(move |delta: i64| {
            let index = step(current.get(), count, delta);
            current.set(index);
            render(index);
        })
    };

    // 单击哪一张就停在哪一张
    render(current.get());

    {
        let go = Rc::clone(&go);
        prev.connect_clicked(move |_| go(-1));
    }
    {
        let go = Rc::clone(&go);
        next.connect_clicked(move |_| go(1));
    }

    // 键盘：← → 切换，Esc 关闭（与详情页的 Esc 语义保持一致：先关掉最上层的东西）
    let keys = gtk::EventControllerKey::new();
    {
        let go = Rc::clone(&go);
        let window_for_keys = window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            match key {
                gtk::gdk::Key::Left => go(-1),
                gtk::gdk::Key::Right => go(1),
                gtk::gdk::Key::Escape => window_for_keys.close(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
    }
    window.add_controller(keys);

    window.present();
}

#[cfg(test)]
mod tests {
    use super::step;

    #[test]
    fn step_clamps_at_both_ends() {
        assert_eq!(step(0, 3, -1), 0, "第一张再往前仍是第一张");
        assert_eq!(step(0, 3, 1), 1);
        assert_eq!(step(2, 3, 1), 2, "最后一张再往后仍是最后一张");
        assert_eq!(step(1, 3, -1), 0);
        // 单张图片：两个方向都不动
        assert_eq!(step(0, 1, 1), 0);
        assert_eq!(step(0, 1, -1), 0);
        // 空列表不能 panic
        assert_eq!(step(0, 0, 1), 0);
    }
}
