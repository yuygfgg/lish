#ifndef LISH_SLIRP_H
#define LISH_SLIRP_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct lish_slirp lish_slirp_t;

typedef void (*lish_slirp_output_ready_cb)(void *opaque);

typedef struct lish_slirp_config {
    uint32_t queue_capacity;
    bool disable_host_loopback;
} lish_slirp_config_t;

typedef struct lish_slirp_stats {
    uint64_t frames_from_guest;
    uint64_t bytes_from_guest;
    uint64_t drops_from_guest;
    uint64_t frames_to_guest;
    uint64_t bytes_to_guest;
    uint64_t drops_to_guest;
    uint32_t queued_from_guest;
    uint32_t queued_to_guest;
} lish_slirp_stats_t;

lish_slirp_t *lish_slirp_create(const lish_slirp_config_t *config,
                                lish_slirp_output_ready_cb output_ready,
                                void *output_opaque,
                                char *error,
                                size_t error_capacity);

/* Returns 1 when accepted, 0 when the bounded queue is full, and -1 for bad input. */
int lish_slirp_input(lish_slirp_t *slirp, const uint8_t *frame, size_t length);

/* Returns 1 for one frame, 0 when empty, and -1 when the output buffer is too small. */
int lish_slirp_next_output(lish_slirp_t *slirp,
                           uint8_t *frame,
                           size_t capacity,
                           size_t *length);

/* All addresses are IPv4 text. Ports use host byte order. */
int lish_slirp_add_host_forward(lish_slirp_t *slirp,
                                bool udp,
                                const char *host_address,
                                uint16_t host_port,
                                const char *guest_address,
                                uint16_t guest_port);

int lish_slirp_remove_host_forward(lish_slirp_t *slirp,
                                   bool udp,
                                   const char *host_address,
                                   uint16_t host_port);

void lish_slirp_get_stats(lish_slirp_t *slirp, lish_slirp_stats_t *stats);

/* Stops the poll thread and unblocks pending operations. The object remains valid. */
void lish_slirp_stop(lish_slirp_t *slirp);
void lish_slirp_destroy(lish_slirp_t *slirp);

#ifdef __cplusplus
}
#endif

#endif
