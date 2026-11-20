#ifndef PHOTOCRAFT_BRIDGING_H
#define PHOTOCRAFT_BRIDGING_H

#include <stdint.h>
#include <stddef.h>

typedef struct PCCallbacks {
    void *user;
    void (*open_dialog)(void *user, int multiple, int pictures);
    void (*save_dialog)(void *user, const char *name);
    int (*write_file)(void *user, const char *path, const uint8_t *data, size_t len);
    void (*open_url)(void *user, const char *url);
    void (*copy_text)(void *user, const char *text);
} PCCallbacks;

void *pc_create(void *metal_layer, uint32_t width_px, uint32_t height_px, float scale, PCCallbacks cb);
void pc_destroy(void *h);
void pc_resize(void *h, uint32_t width_px, uint32_t height_px, float scale);
void pc_set_dark(void *h, int dark);
void pc_set_focused(void *h, int focused);
void pc_set_ui_scale(void *h, float zoom);
void pc_pointer_moved(void *h, float x, float y);
void pc_pointer_gone(void *h);
void pc_pointer_button(void *h, float x, float y, int button, int pressed, uint32_t mods);
void pc_pen(void *h, float x, float y, int phase, float pressure, float tilt_x, float tilt_y, float rotation, uint32_t mods);
void pc_pen_hover(void *h, float x, float y, float tilt_x, float tilt_y, float rotation);
void pc_pen_hover_end(void *h);
void pc_finger(void *h, int phase, float x, float y, uint32_t mods);
void pc_long_press(void *h, float x, float y);
void pc_click(void *h, float x, float y, int button, uint32_t mods);
void pc_set_touch_shift(void *h, int on);
void pc_scroll(void *h, float dx, float dy, uint32_t mods);
void pc_zoom(void *h, float factor, float x, float y);
void pc_rotate(void *h, float radians);
void pc_key(void *h, uint32_t hid, int pressed, int repeat, uint32_t mods);
void pc_text(void *h, const char *utf8);
void pc_copy(void *h);
void pc_cut(void *h);
void pc_paste(void *h, const char *utf8);
void pc_open_file(void *h, const char *name, const uint8_t *data, size_t len);
void pc_dialog_file(void *h, const char *name, const uint8_t *data, size_t len);
void pc_dialog_path(void *h, const char *path);
void pc_dialog_end(void *h, int ok);
void pc_save_dialog_end(void *h, const char *path);
int pc_frame(void *h);
void pc_save_state(void *h);
int pc_is_dirty(void *h);

#endif
