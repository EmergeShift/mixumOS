// input.c —— 输入注入：内建 wayland 客户端 + virtual-pointer/keyboard 协议。
// mixcomp 连自己的 wayland 套接字，用 zwp_virtual_keyboard_v1 与
// zwlr_virtual_pointer_v1 创建虚拟设备；wlroots 为其生成标准输入事件，
// main.c 把事件路由到目标窗口的 seat。只依赖公开协议，行为与外部
// 输入工具（如 wtype/ydotool 的 wayland 路径）一致。
//
// 协议消息一律走 wayland-scanner 生成的桩函数（头文件由 Makefile 从
// protocol/*.xml 现场生成），不再手搓 wl_interface 表：new_id 的变参
// 占位（固定接口传 NULL）、opcode 顺序、签名串都由生成器保证与 XML
// 逐字节一致。手搓表曾因 create_virtual_pointer 签名写反（"no"，上游
// 是 "on"）导致越界读变参、在 wl_proxy_marshal_flags 里随机段错误。
#include <errno.h>
#include <fcntl.h>
#include <json-c/json.h>
#include <linux/input-event-codes.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client-core.h>
#include <wayland-client-protocol.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon-names.h>
#include <xkbcommon/xkbcommon.h>

#include "mixcomp.h"
#include "virtual-keyboard-client-protocol.h"
#include "wlr-virtual-pointer-client-protocol.h"

#define KEYMAP_FORMAT_XKB_V1 1
#define POINTER_AXIS_VERTICAL 0
#define AXIS_SOURCE_WHEEL 0
#define KEY_STATE_RELEASED 0
#define KEY_STATE_PRESSED 1

struct mixcomp_window;
static struct mixcomp_window *find_window(struct mixcomp_server *s, int id);

static struct mixcomp_window *find_window(struct mixcomp_server *s, int id) {
	struct mixcomp_window *w;
	wl_list_for_each(w, &s->windows, link) {
		if ((int)w->id == id) {
			return w;
		}
	}
	return NULL;
}

// ---------- 客户端状态 ----------

struct inj_client {
	struct wl_display *display;
	struct wl_registry *registry;
	struct wl_seat *seat;
	struct zwlr_virtual_pointer_manager_v1 *vp_mgr;
	struct zwp_virtual_keyboard_manager_v1 *kb_mgr;
	struct zwlr_virtual_pointer_v1 *pointer;
	struct zwp_virtual_keyboard_v1 *keyboard;
	struct xkb_context *xkb_ctx;
	struct xkb_keymap *xkb_keymap;
	struct xkb_state *xkb_state;
};

static struct inj_client inj = {0};

static uint32_t now_ms(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint32_t)((uint64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

static void seat_capabilities(void *data, struct wl_seat *seat, uint32_t caps) {
	(void)data; (void)seat; (void)caps; // 注入不需要 seat 能力协商
}

// seat v5 起服务端必发 name 事件；libwayland 1.22+ 对 NULL 监听器槽位
// 直接 abort（wl_closure_invoke），必须挂处理函数。
static void seat_name(void *data, struct wl_seat *seat, const char *name) {
	(void)data; (void)seat; (void)name;
}

static const struct wl_seat_listener seat_listener = {
	.capabilities = seat_capabilities,
	.name = seat_name,
};

static void maybe_create_devices(struct mixcomp_server *s);
static bool keymap_upload(struct mixcomp_server *s);

static void registry_global(void *data, struct wl_registry *reg, uint32_t name,
		const char *iface, uint32_t version) {
	struct mixcomp_server *s = data;
	if (strcmp(iface, "wl_seat") == 0 && inj.seat == NULL) {
		inj.seat = wl_registry_bind(reg, name, &wl_seat_interface,
				version < 5 ? version : 5);
		wl_seat_add_listener(inj.seat, &seat_listener, s);
	} else if (strcmp(iface, zwlr_virtual_pointer_manager_v1_interface.name) == 0 &&
			inj.vp_mgr == NULL) {
		uint32_t v = version < (uint32_t)zwlr_virtual_pointer_manager_v1_interface.version ?
				version : (uint32_t)zwlr_virtual_pointer_manager_v1_interface.version;
		inj.vp_mgr = wl_registry_bind(reg, name,
				&zwlr_virtual_pointer_manager_v1_interface, v);
	} else if (strcmp(iface, zwp_virtual_keyboard_manager_v1_interface.name) == 0 &&
			inj.kb_mgr == NULL) {
		uint32_t v = version < (uint32_t)zwp_virtual_keyboard_manager_v1_interface.version ?
				version : (uint32_t)zwp_virtual_keyboard_manager_v1_interface.version;
		inj.kb_mgr = wl_registry_bind(reg, name,
				&zwp_virtual_keyboard_manager_v1_interface, v);
	}
	// 全异步：managers 就绪即在事件循环内创建设备，绝不阻塞合成器主循环。
	maybe_create_devices(s);
}

static bool devices_created = false;

static void maybe_create_devices(struct mixcomp_server *s) {
	if (devices_created || inj.seat == NULL || inj.vp_mgr == NULL || inj.kb_mgr == NULL) {
		return;
	}
	devices_created = true;
	wlr_log(WLR_INFO, "创建虚拟输入设备: seat=%p vp_mgr=%p kb_mgr=%p",
			(void *)inj.seat, (void *)inj.vp_mgr, (void *)inj.kb_mgr);
	// 生成桩内部按 new_id 惯例创建代理并填参，版本取 manager 协商值。
	inj.pointer = zwlr_virtual_pointer_manager_v1_create_virtual_pointer(
			inj.vp_mgr, inj.seat);
	inj.keyboard = zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(
			inj.kb_mgr, inj.seat);
	if (inj.pointer == NULL || inj.keyboard == NULL) {
		wlr_log(WLR_ERROR, "虚拟输入设备创建失败（pointer=%p keyboard=%p）",
				(void *)inj.pointer, (void *)inj.keyboard);
		devices_created = false;
		return;
	}
	if (!keymap_upload(s)) {
		wlr_log(WLR_ERROR, "keymap 上传失败，键盘注入不可用");
	}
	wl_display_flush(inj.display);
	wlr_log(WLR_INFO, "注入客户端就绪：virtual pointer + keyboard 已创建");
}

static void registry_global_remove(void *data, struct wl_registry *reg, uint32_t name) {
	(void)data; (void)reg; (void)name;
}

static const struct wl_registry_listener registry_listener = {
	.global = registry_global,
	.global_remove = registry_global_remove,
};

// keymap_upload —— 生成默认 xkb keymap 并发送给虚拟键盘。
static bool keymap_upload(struct mixcomp_server *s) {
	if (inj.keyboard == NULL) {
		return false;
	}
	if (inj.xkb_ctx == NULL) {
		inj.xkb_ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
	}
	struct xkb_rule_names names = {
		.rules = "evdev", .model = "pc105", .layout = "us",
		.variant = "", .options = "",
	};
	inj.xkb_keymap = xkb_keymap_new_from_names(inj.xkb_ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
	if (inj.xkb_keymap == NULL) {
		return false;
	}
	const char *str = xkb_keymap_get_as_string(inj.xkb_keymap, XKB_KEYMAP_FORMAT_TEXT_V1);
	size_t size = strlen(str) + 1;
	int fd = memfd_create("mixum-keymap", MFD_CLOEXEC);
	if (fd < 0) {
		return false;
	}
	if (ftruncate(fd, (off_t)size) != 0 || write(fd, str, size) != (ssize_t)size) {
		close(fd);
		return false;
	}
	lseek(fd, 0, SEEK_SET);
	// keymap(format, fd, size)。libwayland 在封送（wl_closure_marshal）时
	// 就会 dup 该 fd，因此请求发出后立即 close 是安全的；此前曾把 close
	// 提前/参数错序，dup 拿到错误 fd 导致 keymap 请求被丢弃。
	zwp_virtual_keyboard_v1_keymap(inj.keyboard, KEYMAP_FORMAT_XKB_V1, fd,
			(uint32_t)size);
	close(fd);
	inj.xkb_state = xkb_state_new(inj.xkb_keymap);
	wl_display_flush(s->inj_display);
	return inj.xkb_state != NULL;
}

// keycode_for_keysym —— 反查 keysym 对应的 evdev 码。
static uint32_t keycode_for_keysym(const char *name) {
	xkb_keysym_t sym = xkb_keysym_from_name(name, XKB_KEYSYM_CASE_INSENSITIVE);
	if (sym == XKB_KEY_NoSymbol || inj.xkb_keymap == NULL) {
		return 0;
	}
	xkb_keycode_t min = xkb_keymap_min_keycode(inj.xkb_keymap);
	xkb_keycode_t max = xkb_keymap_max_keycode(inj.xkb_keymap);
	for (xkb_keycode_t kc = min; kc <= max; kc++) {
		const xkb_keysym_t *syms;
		int n = xkb_keymap_key_get_syms_by_level(inj.xkb_keymap, kc, 0, 0, &syms);
		for (int i = 0; i < n; i++) {
			if (syms[i] == sym) {
				return (uint32_t)kc - 8; // xkb → evdev
			}
		}
	}
	return 0;
}

// send_modifiers —— 客户端侧 xkb 状态序列化后同步给键盘设备。
static void send_modifiers(struct mixcomp_server *s) {
	if (inj.keyboard == NULL || inj.xkb_state == NULL) {
		return;
	}
	uint32_t dep = xkb_state_serialize_mods(inj.xkb_state, XKB_STATE_MODS_DEPRESSED);
	uint32_t lat = xkb_state_serialize_mods(inj.xkb_state, XKB_STATE_MODS_LATCHED);
	uint32_t loc = xkb_state_serialize_mods(inj.xkb_state, XKB_STATE_MODS_LOCKED);
	uint32_t grp = xkb_state_serialize_layout(inj.xkb_state, XKB_STATE_LAYOUT_EFFECTIVE);
	zwp_virtual_keyboard_v1_modifiers(inj.keyboard, dep, lat, loc, grp);
	wl_display_flush(s->inj_display);
}

static void key_event(struct mixcomp_server *s, uint32_t evdev_key, bool press) {
	if (inj.keyboard == NULL || inj.xkb_state == NULL) {
		return;
	}
	uint32_t t = now_ms();
	xkb_state_update_key(inj.xkb_state, (xkb_keycode_t)(evdev_key + 8),
			press ? XKB_KEY_DOWN : XKB_KEY_UP);
	zwp_virtual_keyboard_v1_key(inj.keyboard, t, evdev_key,
			press ? KEY_STATE_PRESSED : KEY_STATE_RELEASED);
	send_modifiers(s);
}

// ---------- 注入入口（mixumd 控制消息） ----------

static uint32_t mod_keycode(const char *m) {
	if (strcmp(m, "ctrl") == 0) {
		return KEY_LEFTCTRL;
	}
	if (strcmp(m, "alt") == 0) {
		return KEY_LEFTALT;
	}
	if (strcmp(m, "shift") == 0) {
		return KEY_LEFTSHIFT;
	}
	if (strcmp(m, "logo") == 0) {
		return KEY_LEFTMETA;
	}
	return 0;
}

static void inject_motion(struct mixcomp_server *s, struct mixcomp_window *w,
		double x, double y) {
	if (inj.pointer == NULL) {
		return;
	}
	s->focus_window = w; // 事件路由目标
	s->inj_width = w->width;
	s->inj_height = w->height;
	zwlr_virtual_pointer_v1_motion_absolute(inj.pointer, now_ms(),
			(uint32_t)x, (uint32_t)y, (uint32_t)w->width, (uint32_t)w->height);
	zwlr_virtual_pointer_v1_frame(inj.pointer);
	wl_display_flush(s->inj_display);
}

static void inject_button(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *button, const char *action) {
	if (inj.pointer == NULL) {
		return;
	}
	s->focus_window = w;
	uint32_t code = strcmp(button, "right") == 0 ? BTN_RIGHT :
			strcmp(button, "middle") == 0 ? BTN_MIDDLE : BTN_LEFT;
	uint32_t t = now_ms();
	if (strcmp(action, "press") == 0 || strcmp(action, "click") == 0) {
		zwlr_virtual_pointer_v1_button(inj.pointer, t, code, KEY_STATE_PRESSED);
		zwlr_virtual_pointer_v1_frame(inj.pointer);
	}
	if (strcmp(action, "release") == 0 || strcmp(action, "click") == 0) {
		zwlr_virtual_pointer_v1_button(inj.pointer, t, code, KEY_STATE_RELEASED);
		zwlr_virtual_pointer_v1_frame(inj.pointer);
	}
	wl_display_flush(s->inj_display);
}

static void inject_axis(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *action) {
	if (inj.pointer == NULL) {
		return;
	}
	s->focus_window = w;
	double delta = strcmp(action, "scroll_up") == 0 ? -1.0 : 1.0;
	uint32_t t = now_ms();
	zwlr_virtual_pointer_v1_axis_source(inj.pointer, AXIS_SOURCE_WHEEL);
	zwlr_virtual_pointer_v1_axis(inj.pointer, t, POINTER_AXIS_VERTICAL,
			wl_fixed_from_double(delta));
	zwlr_virtual_pointer_v1_axis_discrete(inj.pointer, t, POINTER_AXIS_VERTICAL,
			wl_fixed_from_double(delta), (int32_t)(delta * 10));
	zwlr_virtual_pointer_v1_frame(inj.pointer);
	wl_display_flush(s->inj_display);
}

static void inject_key(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *keysym_name, uint32_t raw_key, struct json_object *mods,
		const char *action) {
	if (inj.keyboard == NULL) {
		return;
	}
	s->focus_window = w;
	uint32_t key = raw_key;
	if (key == 0 && keysym_name != NULL) {
		key = keycode_for_keysym(keysym_name);
	}
	if (key == 0) {
		return;
	}
	bool press = strcmp(action, "press") == 0 || strcmp(action, "click") == 0;
	bool release = strcmp(action, "release") == 0 || strcmp(action, "click") == 0;

	// 修饰键：按住 → 敲键 → 松开。
	uint32_t mod_keys[4];
	int nmods = 0;
	if (mods != NULL && json_object_is_type(mods, json_type_array)) {
		size_t n = json_object_array_length(mods);
		for (size_t i = 0; i < n && nmods < 4; i++) {
			uint32_t mk = mod_keycode(json_object_get_string(json_object_array_get_idx(mods, i)));
			if (mk) {
				mod_keys[nmods++] = mk;
			}
		}
	}
	bool click = press && release;
	if (click) {
		for (int i = 0; i < nmods; i++) {
			key_event(s, mod_keys[i], true);
		}
		key_event(s, key, true);
		key_event(s, key, false);
		for (int i = nmods - 1; i >= 0; i--) {
			key_event(s, mod_keys[i], false);
		}
	} else {
		if (press) {
			key_event(s, key, true);
		}
		if (release) {
			key_event(s, key, false);
		}
	}
}

void input_handle_json(struct mixcomp_server *s, const char *json, size_t len) {
	(void)len;
	json_object *root = json_tokener_parse(json);
	if (root == NULL) {
		return;
	}
	json_object *jt = NULL, *jid = NULL, *jkind = NULL;
	json_object_object_get_ex(root, "type", &jt);
	json_object_object_get_ex(root, "id", &jid);
	json_object_object_get_ex(root, "kind", &jkind);
	const char *type = jt ? json_object_get_string(jt) : "";
	int id = jid ? json_object_get_int(jid) : 0;
	const char *kind = jkind ? json_object_get_string(jkind) : "";

	if (strcmp(type, "input.mode") == 0) {
		// 接管模式变化：v1 由 mixumd 仲裁，合成器仅记录。
		json_object_put(root);
		return;
	}
	struct mixcomp_window *w = find_window(s, id);
	if (w == NULL || strcmp(type, "input") != 0) {
		json_object_put(root);
		return;
	}
	json_object *jx = NULL, *jy = NULL, *jbtn = NULL, *jact = NULL;
	json_object *jkey = NULL, *jksym = NULL, *jmods = NULL, *jtext = NULL;
	json_object_object_get_ex(root, "x", &jx);
	json_object_object_get_ex(root, "y", &jy);
	json_object_object_get_ex(root, "button", &jbtn);
	json_object_object_get_ex(root, "action", &jact);
	json_object_object_get_ex(root, "key", &jkey);
	json_object_object_get_ex(root, "keysym", &jksym);
	json_object_object_get_ex(root, "mods", &jmods);
	json_object_object_get_ex(root, "text", &jtext);

	if (strcmp(kind, "pointer_motion") == 0) {
		inject_motion(s, w, jx ? json_object_get_double(jx) : 0,
				jy ? json_object_get_double(jy) : 0);
	} else if (strcmp(kind, "pointer_button") == 0) {
		inject_button(s, w, jbtn ? json_object_get_string(jbtn) : "left",
				jact ? json_object_get_string(jact) : "click");
	} else if (strcmp(kind, "pointer_axis") == 0) {
		inject_axis(s, w, jact ? json_object_get_string(jact) : "scroll_down");
	} else if (strcmp(kind, "key") == 0) {
		uint32_t key = jkey ? (uint32_t)json_object_get_int(jkey) : 0;
		inject_key(s, w, jksym ? json_object_get_string(jksym) : NULL, key,
				jmods, jact ? json_object_get_string(jact) : "click");
	} else if (strcmp(kind, "text") == 0) {
		// 宿主输入法已组合完成；直接交给客户端 text-input。
		if (jtext != NULL && json_object_is_type(jtext, json_type_string)) {
			text_input_commit(s, w, json_object_get_string(jtext));
		}
	}
	json_object_put(root);
}

// ---------- 初始化与事件循环集成 ----------

static int inj_fd_cb(int fd, uint32_t mask, void *data) {
	(void)fd;
	struct mixcomp_server *s = data;
	if (mask & WL_EVENT_READABLE) {
		wl_display_dispatch(inj.display);
	}
	if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) {
		wlr_log(WLR_ERROR, "注入客户端连接断开");
		return -1;
	}
	wl_display_flush(s->inj_display);
	return 0;
}

bool input_init(struct mixcomp_server *s) {
	inj.display = wl_display_connect(NULL); // WAYLAND_DISPLAY 已由 main 设置
	if (inj.display == NULL) {
		wlr_log(WLR_ERROR, "注入客户端无法连接自身 wayland 套接字");
		return false;
	}
	s->inj_display = inj.display;
	inj.registry = wl_display_get_registry(inj.display);
	wl_registry_add_listener(inj.registry, &registry_listener, s);

	// 注册事件源后由事件循环完成 registry/bind/设备创建（不阻塞）。
	// 必须显式 flush 一次，get_registry 的请求才会发出去。
	wl_display_flush(inj.display);
	int fd = wl_display_get_fd(inj.display);
	struct wl_event_loop *loop = wl_display_get_event_loop(s->display);
	s->inj_src = wl_event_loop_add_fd(loop, fd, WL_EVENT_READABLE, inj_fd_cb, s);
	wlr_log(WLR_INFO, "注入客户端已连接，等待设备异步创建");
	return true;
}
