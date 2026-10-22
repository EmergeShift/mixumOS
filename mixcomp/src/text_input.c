// text_input.c —— text-input-v3 文本提交:宿主输入法的成串文本 → 焦点窗口。
// 组合与候选由宿主(浏览器/mixum-ctl 页面)完成,容器内不跑输入法;
// 这里只把最终 committed 文本交给客户端的 text-input 对象。
#include "mixcomp.h"

#include <stdlib.h>
#include <string.h>

#include <wlr/types/wlr_text_input_v3.h>

struct mixcomp_text_input {
	struct wl_list link;
	struct mixcomp_server *server;
	struct wlr_text_input_v3 *ti;
	struct mixcomp_window *window; // enable 时的焦点窗口
	bool enabled;
	struct wl_listener enable;
	struct wl_listener disable;
	struct wl_listener destroy;
};

// 同一 Wayland client 才会收到进入;每个客户端一个 text-input 对象。
static bool same_client(struct mixcomp_text_input *ti,
		struct mixcomp_window *w) {
	if (w == NULL || w->surface == NULL || w->surface->resource == NULL) {
		return false;
	}
	struct wl_client *a = wl_resource_get_client(ti->ti->resource);
	struct wl_client *b = wl_resource_get_client(w->surface->resource);
	return a != NULL && a == b;
}

static void enter_if_focused(struct mixcomp_text_input *ti) {
	if (ti->window == NULL || ti->window->surface == NULL) {
		return;
	}
	if (ti->ti->focused_surface == ti->window->surface) {
		return;
	}
	wlr_log(WLR_DEBUG, "text-input enter 窗口 %u", ti->window->id);
	wlr_text_input_v3_send_enter(ti->ti, ti->window->surface);
}

static void enable_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_text_input *ti = wl_container_of(listener, ti, enable);
	ti->enabled = true;
	ti->window = ti->server->focus_window;
	wlr_log(WLR_DEBUG, "text-input enable(焦点窗口 %u)",
			ti->window != NULL ? ti->window->id : 0);
	enter_if_focused(ti);
}

static void disable_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_text_input *ti = wl_container_of(listener, ti, disable);
	ti->enabled = false;
	ti->window = NULL;
	wlr_log(WLR_DEBUG, "text-input disable");
	if (ti->ti->focused_surface != NULL) {
		wlr_text_input_v3_send_leave(ti->ti);
	}
}

static void destroy_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_text_input *ti = wl_container_of(listener, ti, destroy);
	wl_list_remove(&ti->enable.link);
	wl_list_remove(&ti->disable.link);
	wl_list_remove(&ti->destroy.link);
	wl_list_remove(&ti->link);
	free(ti);
}

static void new_text_input_cb(struct wl_listener *listener, void *data) {
	struct mixcomp_server *s =
		wl_container_of(listener, s, new_text_input);
	struct wlr_text_input_v3 *text_input = data;
	struct mixcomp_text_input *ti = calloc(1, sizeof(*ti));
	if (ti == NULL) {
		wlr_log(WLR_ERROR, "text-input 记录分配失败");
		return;
	}
	ti->server = s;
	ti->ti = text_input;
	ti->enable.notify = enable_cb;
	wl_signal_add(&text_input->events.enable, &ti->enable);
	ti->disable.notify = disable_cb;
	wl_signal_add(&text_input->events.disable, &ti->disable);
	ti->destroy.notify = destroy_cb;
	wl_signal_add(&text_input->events.destroy, &ti->destroy);
	wl_list_insert(&s->text_inputs, &ti->link);
	wlr_log(WLR_DEBUG, "text-input 对象创建");
	if (same_client(ti, s->focus_window)) {
		ti->window = s->focus_window;
		enter_if_focused(ti);
	}
}

void text_input_init(struct mixcomp_server *s) {
	wl_list_init(&s->text_inputs);
	s->text_input_mgr = wlr_text_input_manager_v3_create(s->display);
	if (s->text_input_mgr == NULL) {
		wlr_log(WLR_ERROR, "text-input-v3 管理器创建失败");
		return;
	}
	s->new_text_input.notify = new_text_input_cb;
	wl_signal_add(&s->text_input_mgr->events.text_input, &s->new_text_input);
}

void text_input_focus(struct mixcomp_server *s, struct mixcomp_window *w) {
	struct mixcomp_text_input *ti;
	wl_list_for_each(ti, &s->text_inputs, link) {
		if (same_client(ti, w)) {
			// 客户端要收到 enter 才会 enable（foot 1.19 即如此），
			// 因此焦点变化时主动下发，而不等 enable 事件。
			ti->window = w;
			enter_if_focused(ti);
		} else {
			ti->window = NULL;
			if (ti->ti->focused_surface != NULL) {
				wlr_text_input_v3_send_leave(ti->ti);
			}
		}
	}
}

void text_input_commit(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *text) {
	if (text == NULL || text[0] == '\0') {
		return;
	}
	struct mixcomp_text_input *ti;
	wl_list_for_each(ti, &s->text_inputs, link) {
		if (!ti->enabled || ti->window != w) {
			continue;
		}
		wlr_log(WLR_DEBUG, "text-input commit 窗口 %u: %u 字节", w->id,
				(unsigned)strlen(text));
		wlr_text_input_v3_send_commit_string(ti->ti, text);
		wlr_text_input_v3_send_done(ti->ti);
		return;
	}
	wlr_log(WLR_DEBUG, "窗口 %u 无活动 text-input,文本输入丢弃",
			w != NULL ? w->id : 0);
}

void text_input_forget_window(struct mixcomp_server *s,
		struct mixcomp_window *w) {
	struct mixcomp_text_input *ti;
	wl_list_for_each(ti, &s->text_inputs, link) {
		if (ti->window != w) {
			continue;
		}
		ti->window = NULL;
		if (ti->ti->focused_surface != NULL) {
			wlr_text_input_v3_send_leave(ti->ti);
		}
	}
}
