// window.c —— 每窗口隔离捕获：不建输出，直接读 client buffer。
// 帧回调 = 每窗口定时器；同时向客户端发 frame_done 维持其渲染节拍。
#include <string.h>
#include <time.h>
#include <drm_fourcc.h>
#include <wlr/types/wlr_buffer.h>
#include <wlr/render/wlr_texture.h>

#include "mixcomp.h"

static uint8_t *frame_buf = NULL;
static size_t frame_buf_size = 0;

static void window_ensure_title(struct mixcomp_window *w) {
	struct wlr_xdg_toplevel *t = w->xdg->toplevel;
	if (t == NULL) {
		return;
	}
	bool changed = false;
	if (t->title != NULL && strncmp(w->title, t->title, sizeof(w->title) - 1) != 0) {
		snprintf(w->title, sizeof(w->title), "%s", t->title);
		changed = true;
	}
	if (t->app_id != NULL && strncmp(w->app_id, t->app_id, sizeof(w->app_id) - 1) != 0) {
		snprintf(w->app_id, sizeof(w->app_id), "%s", t->app_id);
		changed = true;
	}
	if (changed) {
		stream_send_window(w->server, w, "window.update");
	}
}

static void set_title_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_window *w = wl_container_of(listener, w, set_title);
	window_ensure_title(w);
}

static void set_app_id_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_window *w = wl_container_of(listener, w, set_app_id);
	window_ensure_title(w);
}

static void commit_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_window *w = wl_container_of(listener, w, commit);
	struct wlr_xdg_surface *x = w->xdg;
	if (x == NULL || x->surface == NULL) {
		return;
	}
	// wlroots 0.18 语义：xdg_surface 的 initial commit 后合成器必须回一个
	// 空 configure，客户端 ack 后才允许贴第一帧缓冲（tinywl 同款做法，
	// 0,0 表示尺寸由客户端自选，mixcomp 不做摆放）。
	if (x->initial_commit) {
		wlr_xdg_toplevel_set_size(x->toplevel, 0, 0);
		return;
	}
	int wpx = x->surface->current.width, hpx = x->surface->current.height;
	if (wpx > 0 && hpx > 0 && (wpx != w->width || hpx != w->height)) {
		w->width = wpx;
		w->height = hpx;
		stream_send_window(w->server, w, "window.update");
	}
	window_ensure_title(w);
}

static void unmap_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_window *w = wl_container_of(listener, w, unmap);
	stream_send_window(w->server, w, "window.close");
}

static void destroy_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_window *w = wl_container_of(listener, w, destroy);
	if (w->timer != NULL) {
		wl_event_source_remove(w->timer);
	}
#ifdef HAVE_OPENH264
	if (w->h264 != NULL) {
		h264_close(w->h264);
		w->h264 = NULL;
	}
#endif
	wl_list_remove(&w->map.link);
	wl_list_remove(&w->unmap.link);
	wl_list_remove(&w->commit.link);
	wl_list_remove(&w->destroy.link);
	wl_list_remove(&w->set_title.link);
	wl_list_remove(&w->set_app_id.link);
	wl_list_remove(&w->link);
	free(w->prev);
	free(w);
}

// timer_cb —— 帧节拍：捕获（如有订阅者）+ frame_done（永远）。
// wayland 定时器单次触发，回调里重新 arm 到下一个节拍。
static int timer_cb(void *data) {
	struct mixcomp_window *w = data;
	if (w->server->snapshot_frozen) {
		// 冻结时定时器已被 disarm；这里只防御已排队的回调。
		return 0;
	}
	if (w->timer != NULL) {
		// 按窗口 fps 重挂节拍（stream.start 可动态调整，缺省 30）。
		wl_event_source_timer_update(w->timer, w->fps > 0 ? 1000 / w->fps : 33);
	}
	if (w->surface == NULL || !w->surface->mapped) {
		return 0;
	}
	// frame_done：客户端没有它就会节流停帧，必须无条件发送。
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	wlr_surface_send_frame_done(w->surface, &now);

	if (w->want_stream && w->surface->buffer != NULL) {
		struct wlr_client_buffer *cb = w->surface->buffer;
		int width = cb->base.width, height = cb->base.height;
		if (width > 0 && height > 0 && width < 8192 && height < 8192) {
			size_t need = (size_t)width * height * 4;
			if (frame_buf_size < need) {
				free(frame_buf);
				frame_buf = malloc(need);
				frame_buf_size = frame_buf ? need : 0;
			}
			if (frame_buf != NULL) {
				struct wlr_texture *tex = cb->texture;
				struct wlr_texture *owned = NULL;
				if (tex == NULL) {
					owned = wlr_texture_from_buffer(w->server->renderer, &cb->base);
					tex = owned;
				}
				if (tex != NULL) {
					uint32_t fmt = wlr_texture_preferred_read_format(tex);
					struct wlr_box src = {0, 0, width, height};
					struct wlr_texture_read_pixels_options opts = {
						.data = frame_buf,
						.format = fmt,
						.stride = (uint32_t)width * 4,
						.src_box = src,
					};
					if (wlr_texture_read_pixels(tex, &opts)) {
						stream_send_frame(w->server, w, frame_buf,
								(int)width * 4, width, height, fmt);
					} else {
						wlr_log(WLR_ERROR, "窗口 %u 读像素失败 fmt=%d", w->id, (int)fmt);
					}
					if (owned != NULL) {
						wlr_texture_destroy(owned);
					}
				} else {
					wlr_log(WLR_ERROR, "窗口 %u 无可读纹理", w->id);
				}
			}
		}
	} else if (w->want_stream) {
		wlr_log(WLR_ERROR, "窗口 %u 捕获跳过: buffer=%p mapped=%d", w->id,
				(void *)w->surface->buffer, w->surface->mapped);
	}
	return 0; // 继续按既有间隔触发
}

// 快照冻结联动：暂停/恢复全部窗口的捕获节拍。
// disarm 后不再有 timer 回调，因此既不读像素也不发 frame_done；
// thaw 时按各窗口当前 fps 重新 arm。冻结期间新映射的窗口由 map_cb 负责不启动节拍。
void windows_set_capture_paused(struct mixcomp_server *s, bool paused) {
	s->snapshot_frozen = paused;
	struct mixcomp_window *w;
	wl_list_for_each(w, &s->windows, link) {
		if (w->timer == NULL) {
			continue;
		}
		if (paused) {
			wl_event_source_timer_update(w->timer, 0);
		} else {
			wl_event_source_timer_update(w->timer, w->fps > 0 ? 1000 / w->fps : 33);
		}
	}
}

static void map_cb(struct wl_listener *listener, void *data) {
	(void)data;
	struct mixcomp_window *w = wl_container_of(listener, w, map);
	struct mixcomp_server *s = w->server;

	// 每窗口独立帧节拍定时器（缺省 30fps，stream.start 可动态调整）。
	w->timer = wl_event_loop_add_timer(wl_display_get_event_loop(s->display),
			timer_cb, w);
	if (w->timer != NULL) {
		int delay = s->snapshot_frozen ? 0 : (w->fps > 0 ? 1000 / w->fps : 33);
		wl_event_source_timer_update(w->timer, delay);
	}
	stream_send_window(s, w, "window.open");
	wlr_log(WLR_INFO, "窗口已隔离: id=%u app=%s title=%s %dx%d",
			w->id, w->app_id, w->title, w->width, w->height);
}

void window_create(struct mixcomp_server *s, struct wlr_xdg_surface *xdg) {
	struct mixcomp_window *w = calloc(1, sizeof(*w));
	if (w == NULL) {
		return;
	}
	w->server = s;
	w->xdg = xdg;
	w->surface = xdg->surface;
	w->id = ++s->next_window_id;
	w->width = 1280;
	w->height = 800;
	w->fps = 30;
	w->jpeg_quality = s->jpeg_quality;
	if (xdg->toplevel->app_id != NULL) {
		snprintf(w->app_id, sizeof(w->app_id), "%s", xdg->toplevel->app_id);
	}
	if (xdg->toplevel->title != NULL) {
		snprintf(w->title, sizeof(w->title), "%s", xdg->toplevel->title);
	}

	wl_list_insert(&s->windows, &w->link);
	w->map.notify = map_cb;
	wl_signal_add(&xdg->surface->events.map, &w->map);
	w->unmap.notify = unmap_cb;
	wl_signal_add(&xdg->surface->events.unmap, &w->unmap);
	w->commit.notify = commit_cb;
	wl_signal_add(&xdg->surface->events.commit, &w->commit);
	w->destroy.notify = destroy_cb;
	wl_signal_add(&xdg->events.destroy, &w->destroy);
	if (xdg->toplevel != NULL) {
		w->set_title.notify = set_title_cb;
		wl_signal_add(&xdg->toplevel->events.set_title, &w->set_title);
		w->set_app_id.notify = set_app_id_cb;
		wl_signal_add(&xdg->toplevel->events.set_app_id, &w->set_app_id);
	}
	wlr_log(WLR_INFO, "新顶层窗口: id=%u app=%s", w->id, w->app_id);
}
