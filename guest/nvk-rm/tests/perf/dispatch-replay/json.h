/* Minimal JSON reader for dispatch-replay manifests (objects, arrays,
 * strings without unicode escapes beyond \uXXXX as '?', numbers, literals).
 */
#ifndef DR_JSON_H
#define DR_JSON_H

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>

enum json_type { J_NULL, J_BOOL, J_NUM, J_STR, J_ARR, J_OBJ };

struct json {
    enum json_type t;
    double num;
    char *str;          /* J_STR value; also the number's text */
    int b;
    int n;              /* children */
    struct json **kids; /* J_ARR elements, J_OBJ values */
    char **keys;        /* J_OBJ keys */
};

static const char *j_ws(const char *p) { while (*p && isspace((unsigned char)*p)) p++; return p; }

static struct json *j_new(enum json_type t) { struct json *j = calloc(1, sizeof *j); j->t = t; return j; }

static void j_push(struct json *j, char *key, struct json *v)
{
    j->kids = realloc(j->kids, (j->n + 1) * sizeof *j->kids);
    if (j->t == J_OBJ) j->keys = realloc(j->keys, (j->n + 1) * sizeof *j->keys);
    j->kids[j->n] = v;
    if (j->t == J_OBJ) j->keys[j->n] = key;
    j->n++;
}

static const char *j_parse_str(const char *p, char **out)
{
    /* p at opening quote */
    size_t cap = 32, len = 0; char *s = malloc(cap);
    p++;
    while (*p && *p != '"') {
        char c = *p++;
        if (c == '\\') {
            char e = *p++;
            switch (e) {
            case 'n': c = '\n'; break;
            case 't': c = '\t'; break;
            case 'r': c = '\r'; break;
            case 'b': c = '\b'; break;
            case 'f': c = '\f'; break;
            case 'u': c = '?'; for (int i = 0; i < 4 && *p; i++) p++; break;
            default: c = e; break;
            }
        }
        if (len + 2 > cap) { cap *= 2; s = realloc(s, cap); }
        s[len++] = c;
    }
    s[len] = 0;
    if (*p == '"') p++;
    *out = s;
    return p;
}

static const char *j_parse(const char *p, struct json **out)
{
    p = j_ws(p);
    if (*p == '{') {
        struct json *j = j_new(J_OBJ);
        p = j_ws(p + 1);
        while (*p && *p != '}') {
            char *key = NULL;
            if (*p != '"') { *out = NULL; return NULL; }
            p = j_parse_str(p, &key);
            p = j_ws(p);
            if (*p != ':') { *out = NULL; return NULL; }
            struct json *v = NULL;
            p = j_parse(p + 1, &v);
            if (!p) { *out = NULL; return NULL; }
            j_push(j, key, v);
            p = j_ws(p);
            if (*p == ',') p = j_ws(p + 1);
        }
        if (*p == '}') p++;
        *out = j;
        return p;
    }
    if (*p == '[') {
        struct json *j = j_new(J_ARR);
        p = j_ws(p + 1);
        while (*p && *p != ']') {
            struct json *v = NULL;
            p = j_parse(p, &v);
            if (!p) { *out = NULL; return NULL; }
            j_push(j, NULL, v);
            p = j_ws(p);
            if (*p == ',') p = j_ws(p + 1);
        }
        if (*p == ']') p++;
        *out = j;
        return p;
    }
    if (*p == '"') {
        struct json *j = j_new(J_STR);
        p = j_parse_str(p, &j->str);
        *out = j;
        return p;
    }
    if (!strncmp(p, "true", 4)) { struct json *j = j_new(J_BOOL); j->b = 1; *out = j; return p + 4; }
    if (!strncmp(p, "false", 5)) { struct json *j = j_new(J_BOOL); *out = j; return p + 5; }
    if (!strncmp(p, "null", 4)) { *out = j_new(J_NULL); return p + 4; }
    if (*p == '-' || isdigit((unsigned char)*p)) {
        struct json *j = j_new(J_NUM);
        const char *s = p;
        while (*p && (isdigit((unsigned char)*p) || *p == '-' || *p == '+' || *p == '.' || *p == 'e' || *p == 'E')) p++;
        j->str = malloc(p - s + 1); memcpy(j->str, s, p - s); j->str[p - s] = 0;
        j->num = strtod(j->str, NULL);
        *out = j;
        return p;
    }
    *out = NULL;
    return NULL;
}

static struct json *json_parse(const char *text)
{
    struct json *j = NULL;
    if (!j_parse(text, &j)) return NULL;
    return j;
}

static struct json *jget(const struct json *o, const char *key)
{
    if (!o || o->t != J_OBJ) return NULL;
    for (int i = 0; i < o->n; i++) if (!strcmp(o->keys[i], key)) return o->kids[i];
    return NULL;
}

static struct json *jidx(const struct json *a, int i)
{
    if (!a || a->t != J_ARR || i < 0 || i >= a->n) return NULL;
    return a->kids[i];
}

static double jnum(const struct json *o, const char *key, double def)
{
    const struct json *v = jget(o, key);
    if (!v) return def;
    if (v->t == J_NUM) return v->num;
    if (v->t == J_BOOL) return v->b;
    return def;
}

static uint64_t ju64(const struct json *o, const char *key, uint64_t def)
{
    const struct json *v = jget(o, key);
    if (!v) return def;
    if (v->t == J_NUM) return strtoull(v->str, NULL, 10);
    if (v->t == J_STR) return strtoull(v->str, NULL, 0);
    if (v->t == J_BOOL) return v->b;
    return def;
}

static const char *jstr(const struct json *o, const char *key, const char *def)
{
    const struct json *v = jget(o, key);
    return v && v->t == J_STR ? v->str : def;
}

static int jbool(const struct json *o, const char *key, int def)
{
    const struct json *v = jget(o, key);
    return v && v->t == J_BOOL ? v->b : def;
}

#endif
