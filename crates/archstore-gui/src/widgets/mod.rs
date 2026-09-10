//! 控件层。
//!
//! 设计约束（project.md §2.2 ADR / 附录 D #11）：**不使用自引用结构**。
//! 因此每个控件都是"数据行 + 独立渲染函数"：渲染函数接收不可变数据，返回控件树；
//! 控件本身不持有对数据的引用，回调通过参数传入。

pub mod dep_list;
pub mod error_view;
pub mod package_detail_view;
pub mod package_row;
pub mod plan_bar;
pub mod progress_panel;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gio::prelude::*;
use gtk::prelude::*;

use archstore_core::model::{PackageId, PackageSummary};

use crate::icon_cache::IconCache;

/// 远程图标的"占位槽位"：下载完成后需要**原地回填**的 `gtk::Image`。
///
/// 一个包可能同时出现在多个页面（已安装 / 搜索结果 / 详情），所以每个包是一个列表；
/// 用弱引用是因为 `GtkListView` 会回收行控件 —— 控件销毁后槽位自然失效。
///
/// key 的存在本身就表示"该包已有一次下载在途"：同一行被反复 bind、或多个页面
/// 显示同一个包时，不会重复发起网络请求。
pub type IconSlots = RefCell<HashMap<PackageId, Vec<glib::WeakRef<gtk::Image>>>>;

/// 渲染列表行所需的共享上下文（仅主线程使用）。
pub struct RowContext {
    pub icon_size: i32,
    pub icons: Rc<RefCell<IconCache>>,
    /// 远程图标的异步加载器（默认未注入 = 不联网）
    pub loader: RefCell<Option<crate::icon_cache::IconLoader>>,
    /// 见 [`IconSlots`]
    pub slots: Rc<IconSlots>,
}

impl RowContext {
    /// 登记一个"下载完成后要回填"的占位控件。
    ///
    /// 返回 `true` 表示这是该包的第一个槽位 —— 调用方应当发起下载；
    /// `false` 表示已有下载在途。
    pub fn register_icon_slot(&self, id: &PackageId, image: &gtk::Image) -> bool {
        let mut map = self.slots.borrow_mut();
        match map.get_mut(id) {
            Some(list) => {
                list.push(image.downgrade());
                false
            }
            None => {
                map.insert(id.clone(), vec![image.downgrade()]);
                true
            }
        }
    }

    /// 取出并清空某个包的槽位（下载完成时调用）。
    pub fn take_icon_slots(&self, id: &PackageId) -> Vec<glib::WeakRef<gtk::Image>> {
        self.slots.borrow_mut().remove(id).unwrap_or_default()
    }

    /// 放弃某个包的槽位（下载失败或没有加载器时调用，允许下次重试）。
    pub fn drop_icon_slots(&self, id: &PackageId) {
        self.slots.borrow_mut().remove(id);
    }

    /// 在途（等待下载）的图标数量：测试与诊断用。
    pub fn pending_icons(&self) -> usize {
        self.slots.borrow().len()
    }
}

impl std::fmt::Debug for RowContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowContext")
            .field("icon_size", &self.icon_size)
            .field("icons", &self.icons.borrow().len())
            .field("has_loader", &self.has_icon_loader())
            .field("pending_icons", &self.pending_icons())
            .finish()
    }
}

impl RowContext {
    pub fn new(icon_size: i32) -> Self {
        Self {
            icon_size,
            icons: Rc::new(RefCell::new(IconCache::new())),
            loader: RefCell::new(None),
            slots: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// 注入远程图标加载器（由 window 在服务就绪后调用）。
    ///
    /// 未注入时不会发起任何网络请求，因此单元测试与离线场景都不会静默联网。
    pub fn set_icon_loader(&self, loader: crate::icon_cache::IconLoader) {
        *self.loader.borrow_mut() = Some(loader);
    }

    pub fn has_icon_loader(&self) -> bool {
        self.loader.borrow().is_some()
    }
}

/// 把列表数据包装成 gio::ListStore（ListView 的数据模型）。
///
/// 列表用 GtkListView + gio::ListStore（**不用 GtkFlowBox**：FlowBox 在千级条目下性能不可接受）。
pub fn store_from(items: &[PackageSummary]) -> gio::ListStore {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    append_to_store(&store, items);
    store
}

/// 追加数据到已有的 store。
pub fn append_to_store(store: &gio::ListStore, items: &[PackageSummary]) {
    let mut objects: Vec<glib::BoxedAnyObject> = Vec::with_capacity(items.len());
    for item in items {
        objects.push(glib::BoxedAnyObject::new(item.clone()));
    }
    store.splice(store.n_items(), 0, &objects);
}

/// ListStore 中的条目数（u32，与 GTK 的索引类型一致）。
pub fn store_len(store: &gio::ListStore) -> u32 {
    store.n_items()
}

/// 增量 diff 更新（§3.3：不做全量重建，避免滚动位置丢失）。
///
/// 简单但正确的策略：前缀相同则只替换尾部；否则整体替换。
pub fn diff_update(store: &gio::ListStore, items: &[PackageSummary]) {
    let existing = store.n_items();
    let same_prefix = {
        let mut n: u32 = 0;
        while n < existing && (n as usize) < items.len() {
            match item_at(store, n) {
                Some(s) if s == items[n as usize] => n += 1,
                _ => break,
            }
        }
        n
    };
    if same_prefix == existing && existing == items.len() as u32 {
        return;
    }
    let tail: Vec<glib::BoxedAnyObject> = items[same_prefix as usize..]
        .iter()
        .map(|s| glib::BoxedAnyObject::new(s.clone()))
        .collect();
    store.splice(same_prefix, existing - same_prefix, &tail);
}

/// 取出 store 中第 index 项的数据。
pub fn item_at(store: &gio::ListStore, index: u32) -> Option<PackageSummary> {
    let obj = store.item(index)?;
    let boxed = obj.downcast::<glib::BoxedAnyObject>().ok()?;
    let borrowed = boxed.borrow::<PackageSummary>();
    Some(borrowed.clone())
}

/// 取出 ListItem 中携带的数据。
pub fn summary_of(list_item: &gtk::ListItem) -> Option<PackageSummary> {
    let obj = list_item.item()?;
    let boxed = obj.downcast::<glib::BoxedAnyObject>().ok()?;
    let borrowed = boxed.borrow::<PackageSummary>();
    Some(borrowed.clone())
}

/// 构造一个带增量渲染能力的列表视图。
pub fn list_view(store: &gio::ListStore, ctx: &Rc<RowContext>) -> gtk::ListView {
    let selection = gtk::SingleSelection::builder()
        .model(store)
        .autoselect(false)
        .can_unselect(true)
        .build();
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, _| {});
    let ctx_bind = ctx.clone();
    factory.connect_bind(move |_, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(summary) = summary_of(list_item) else {
            return;
        };
        let row = package_row::render(&summary, &ctx_bind);
        list_item.set_child(Some(&row));
    });
    factory.connect_unbind(|_, item| {
        if let Some(list_item) = item.downcast_ref::<gtk::ListItem>() {
            list_item.set_child(None::<&gtk::Widget>);
        }
    });

    let view = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        .single_click_activate(true)
        .vexpand(true)
        .build();
    view.add_css_class("package-list");
    view
}
