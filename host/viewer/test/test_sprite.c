/* SPDX-License-Identifier: GPL-2.0 OR Apache-2.0 */
/*
 * test_sprite.c -- the guest cursor the viewer draws under grab (nb_sprite.h).
 *
 * The rule under test: it is drawn only while every condition holds, and the
 * moment one goes (above all the grab being released) it is not -- and the
 * area it covered is damaged so the compositor repaints it.
 */
#include <stdio.h>

#include "nb_sprite.h"

static int fails, checks;

#define CHECK(c, ...) do { \
        checks++; \
        if (!(c)) { \
            fails++; \
            fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
            fprintf(stderr, __VA_ARGS__); \
            fputc('\n', stderr); \
        } \
    } while (0)

static struct nb_spr_in drawn(void)
{
    return (struct nb_spr_in){
        .grabbed = true, .focused = true, .ptr_in = true, .shown = true,
        .image = true, .pos = true, .frame = true, .ui = false,
    };
}

/* grab -> ungrab -> grab: drawn, not drawn, drawn. */
static void test_grab_ungrab(void)
{
    struct nb_spr_in in = drawn();

    CHECK(nb_spr_want(&in), "grabbed with everything known: drawn");
    in.grabbed = false;
    CHECK(!nb_spr_want(&in), "released: not drawn");
    in.grabbed = true;
    CHECK(nb_spr_want(&in), "grabbed again: drawn");
}

/* Every other condition takes it away on its own. */
static void test_each_condition(void)
{
    struct nb_spr_in in;

    in = drawn(); in.focused = false;
    CHECK(!nb_spr_want(&in), "focus lost: not drawn");
    in = drawn(); in.ptr_in = false;
    CHECK(!nb_spr_want(&in), "pointer left the window: not drawn");
    in = drawn(); in.shown = false;
    CHECK(!nb_spr_want(&in), "the guest hid its cursor: not drawn");
    in = drawn(); in.image = false;
    CHECK(!nb_spr_want(&in), "no image to draw: not drawn");
    in = drawn(); in.pos = false;
    CHECK(!nb_spr_want(&in), "no guest position: not drawn");
    in = drawn(); in.frame = false;
    CHECK(!nb_spr_want(&in), "the placeholder: not drawn");
    in = drawn(); in.ui = true;
    CHECK(!nb_spr_want(&in), "menu or dialog open: not drawn");
}

/* The damage that repaints what it covered: where it was, and where it went. */
static void test_damage(void)
{
    struct nb_rect acc = { 0, 0, 0, 0 };
    struct nb_rect a = { 100, 50, 32, 32 }, b = { 300, 20, 16, 16 };
    struct nb_rect none = { 5, 5, 0, 7 };

    nb_spr_damage_add(&acc, &none);
    CHECK(acc.w == 0 && acc.h == 0, "an empty rect adds nothing");
    nb_spr_damage_add(&acc, &a);
    CHECK(acc.x == 100 && acc.y == 50 && acc.w == 32 && acc.h == 32,
          "the first rect is taken as is: %d,%d %dx%d", acc.x, acc.y, acc.w,
          acc.h);
    nb_spr_damage_add(&acc, &b);
    CHECK(acc.x == 100 && acc.y == 20 && acc.w == 216 && acc.h == 62,
          "the union: %d,%d %dx%d", acc.x, acc.y, acc.w, acc.h);
}

int main(void)
{
    test_grab_ungrab();
    test_each_condition();
    test_damage();
    printf("test_sprite: %d checks, %d failed\n", checks, fails);
    return fails ? 1 : 0;
}
