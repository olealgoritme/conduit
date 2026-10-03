/* SPDX-License-Identifier: Apache-2.0 */
/*
 * net_shim.c -- a narrow C API over the vendored ENet and nanors, so the Rust
 * side never mirrors their struct layouts.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdint.h>
#include <string.h>
#include <sys/socket.h>

#include <enet/enet.h>
#include "rs.h"

/* ------------------------------------------------------------------ ENet */

struct cs_enet_event {
    int type;            /* 0 none, 1 connect, 2 disconnect, 3 receive */
    void *peer;
    uint32_t data;       /* connect: the client's connect data */
    uint8_t channel;
    const uint8_t *packet;
    size_t len;
    void *pkt;           /* free with cs_enet_packet_free */
    char addr[64];       /* peer address */
    uint16_t port;
    char local[64];      /* our address on that connection */
};

static pthread_once_t once = PTHREAD_ONCE_INIT;
static void init_once(void)
{
    enet_initialize();
    reed_solomon_init();
}

void cs_net_init(void)
{
    pthread_once(&once, init_once);
}

/* A host on [::]:port (dual stack). NULL on failure. */
void *cs_enet_host(uint16_t port, size_t max_peers)
{
    ENetAddress a;
    ENetHost *h;

    cs_net_init();
    memset(&a, 0, sizeof(a));
    enet_address_set_host(&a, "::");
    enet_address_set_port(&a, port);
    /* Moonlight opens up to 0x30 channels (one per input kind and pad). */
    h = enet_host_create(AF_INET6, &a, max_peers, 0x30, 0, 0);
    if (h)
        enet_socket_set_option(h->socket, ENET_SOCKOPT_QOS, 1);
    return h;
}

static void addr_str(const ENetAddress *a, char *out, size_t n, uint16_t *port)
{
    const struct sockaddr_storage *ss = &a->address;

    out[0] = 0;
    if (ss->ss_family == AF_INET6) {
        const struct sockaddr_in6 *s6 = (const void *)ss;
        if (IN6_IS_ADDR_V4MAPPED(&s6->sin6_addr))
            inet_ntop(AF_INET, &s6->sin6_addr.s6_addr[12], out, (socklen_t)n);
        else
            inet_ntop(AF_INET6, &s6->sin6_addr, out, (socklen_t)n);
        if (port)
            *port = ntohs(s6->sin6_port);
    } else if (ss->ss_family == AF_INET) {
        const struct sockaddr_in *s4 = (const void *)ss;
        inet_ntop(AF_INET, &s4->sin_addr, out, (socklen_t)n);
        if (port)
            *port = ntohs(s4->sin_port);
    }
}

int cs_enet_service(void *host, uint32_t timeout_ms, struct cs_enet_event *ev)
{
    ENetEvent e;
    int r;

    memset(ev, 0, sizeof(*ev));
    r = enet_host_service(host, &e, timeout_ms);
    if (r <= 0)
        return r;
    ev->peer = e.peer;
    ev->data = e.data;
    ev->channel = e.channelID;
    if (e.peer) {
        addr_str(&e.peer->address, ev->addr, sizeof(ev->addr), &ev->port);
        addr_str(&e.peer->localAddress, ev->local, sizeof(ev->local), NULL);
    }
    switch (e.type) {
    case ENET_EVENT_TYPE_CONNECT: ev->type = 1; break;
    case ENET_EVENT_TYPE_DISCONNECT: ev->type = 2; break;
    case ENET_EVENT_TYPE_RECEIVE:
        ev->type = 3;
        ev->pkt = e.packet;
        ev->packet = e.packet->data;
        ev->len = e.packet->dataLength;
        break;
    default: ev->type = 0; break;
    }
    return 1;
}

void cs_enet_packet_free(void *pkt)
{
    if (pkt)
        enet_packet_destroy(pkt);
}

int cs_enet_send(void *peer, uint8_t channel, const void *data, size_t len, int reliable)
{
    ENetPacket *p = enet_packet_create(data, len, reliable ? ENET_PACKET_FLAG_RELIABLE : 0);

    if (!p)
        return -1;
    if (enet_peer_send(peer, channel, p) < 0) {
        enet_packet_destroy(p);
        return -1;
    }
    return 0;
}

void cs_enet_flush(void *host)
{
    enet_host_flush(host);
}

void cs_enet_disconnect(void *peer, int now)
{
    if (now)
        enet_peer_disconnect_now(peer, 0);
    else
        enet_peer_disconnect_later(peer, 0);
}

void cs_enet_host_destroy(void *host)
{
    if (host)
        enet_host_destroy(host);
}

/* ------------------------------------------------------------------ Reed-Solomon */

/* Fill shards[ds..ds+ps) from shards[0..ds), each `bs` bytes. `parity_override`
 * (may be NULL) replaces the generated parity matrix (ds*ps bytes). */
int cs_rs_encode(int ds, int ps, uint8_t **shards, int bs, const uint8_t *parity_override)
{
    reed_solomon *rs;
    int r;

    cs_net_init();
    rs = reed_solomon_new(ds, ps);
    if (!rs)
        return -1;
    if (parity_override)
        memcpy(rs->p, parity_override, (size_t)ds * (size_t)ps);
    r = reed_solomon_encode(rs, shards, ds + ps, bs);
    reed_solomon_release(rs);
    return r;
}

/* Recover the shards whose mark is 1 (tests; the client does this). */
int cs_rs_decode(int ds, int ps, uint8_t **shards, uint8_t *marks, int bs)
{
    reed_solomon *rs;
    int r;

    cs_net_init();
    rs = reed_solomon_new(ds, ps);
    if (!rs)
        return -1;
    r = reed_solomon_decode(rs, shards, marks, ds + ps, bs);
    reed_solomon_release(rs);
    return r;
}
