#include "lish_slirp.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <libslirp.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define LISH_DEFAULT_QUEUE_CAPACITY 256u
#define LISH_MTU 1500u
#define LISH_MAX_FRAME_SIZE 1600u

struct frame {
    size_t length;
    uint8_t bytes[LISH_MAX_FRAME_SIZE];
};

struct frame_queue {
    struct frame *frames;
    uint32_t capacity;
    uint32_t head;
    uint32_t count;
};

struct lish_timer {
    SlirpTimerCb callback;
    void *callback_opaque;
    int64_t expires_ms;
    bool armed;
    struct lish_timer *next;
};

enum command_kind {
    COMMAND_NONE,
    COMMAND_ADD_FORWARD,
    COMMAND_REMOVE_FORWARD,
};

struct forward_command {
    enum command_kind kind;
    bool udp;
    struct in_addr host_address;
    struct in_addr guest_address;
    uint16_t host_port;
    uint16_t guest_port;
    bool complete;
    int result;
};

struct poll_context {
    struct pollfd *fds;
    size_t count;
    size_t capacity;
};

struct lish_slirp {
    Slirp *slirp;
    SlirpConfig config;
    SlirpCb callbacks;
    pthread_t thread;
    pthread_mutex_t mutex;
    pthread_cond_t command_condition;
    pthread_cond_t output_condition;
    int wake_read;
    int wake_write;
    bool stopping;
    struct frame_queue input;
    struct frame_queue output;
    struct lish_timer *timers;
    struct forward_command command;
    lish_slirp_output_ready_cb output_ready;
    void *output_opaque;
    lish_slirp_stats_t stats;
};

static int64_t monotonic_ns(void) {
    struct timespec value;
    clock_gettime(CLOCK_MONOTONIC, &value);
    return (int64_t)value.tv_sec * 1000000000ll + value.tv_nsec;
}

static void set_error(char *error, size_t capacity, const char *message) {
    if (error != NULL && capacity != 0) {
        snprintf(error, capacity, "%s", message);
    }
}

static bool queue_init(struct frame_queue *queue, uint32_t capacity) {
    queue->frames = calloc(capacity, sizeof(*queue->frames));
    queue->capacity = capacity;
    return queue->frames != NULL;
}

static bool queue_push(struct frame_queue *queue, const uint8_t *bytes, size_t length) {
    if (queue->count == queue->capacity) {
        return false;
    }
    uint32_t index = (queue->head + queue->count) % queue->capacity;
    queue->frames[index].length = length;
    memcpy(queue->frames[index].bytes, bytes, length);
    queue->count++;
    return true;
}

static bool queue_pop(struct frame_queue *queue, struct frame *frame) {
    if (queue->count == 0) {
        return false;
    }
    *frame = queue->frames[queue->head];
    queue->head = (queue->head + 1) % queue->capacity;
    queue->count--;
    return true;
}

static void wake_worker(struct lish_slirp *state) {
    uint8_t byte = 1;
    ssize_t ignored = write(state->wake_write, &byte, sizeof(byte));
    (void)ignored;
}

static void drain_wake_pipe(struct lish_slirp *state) {
    uint8_t bytes[64];
    while (read(state->wake_read, bytes, sizeof(bytes)) > 0) {
    }
}

static slirp_ssize_t send_packet(const void *buffer, size_t length, void *opaque) {
    struct lish_slirp *state = opaque;
    if (length == 0 || length > LISH_MAX_FRAME_SIZE) {
        return -1;
    }
    pthread_mutex_lock(&state->mutex);
    while (state->output.count == state->output.capacity && !state->stopping) {
        /* Apply backpressure instead of dropping a TCP segment. */
        pthread_cond_wait(&state->output_condition, &state->mutex);
    }
    if (state->stopping) {
        pthread_mutex_unlock(&state->mutex);
        return (slirp_ssize_t)length;
    }
    bool was_empty = state->output.count == 0;
    bool accepted = queue_push(&state->output, buffer, length);
    if (accepted) {
        state->stats.frames_to_guest++;
        state->stats.bytes_to_guest += length;
    } else {
        state->stats.drops_to_guest++;
    }
    pthread_mutex_unlock(&state->mutex);

    if (accepted && was_empty && state->output_ready != NULL) {
        state->output_ready(state->output_opaque);
    }
    return accepted ? (slirp_ssize_t)length : 0;
}

static void guest_error(const char *message, void *opaque) {
    (void)opaque;
    fprintf(stderr, "libslirp rejected guest input: %s\n", message);
}

static int64_t clock_get_ns(void *opaque) {
    (void)opaque;
    return monotonic_ns();
}

static void *timer_new(SlirpTimerCb callback, void *callback_opaque, void *opaque) {
    struct lish_slirp *state = opaque;
    struct lish_timer *timer = calloc(1, sizeof(*timer));
    if (timer == NULL) {
        return NULL;
    }
    timer->callback = callback;
    timer->callback_opaque = callback_opaque;
    pthread_mutex_lock(&state->mutex);
    timer->next = state->timers;
    state->timers = timer;
    pthread_mutex_unlock(&state->mutex);
    return timer;
}

static void timer_free(void *timer_pointer, void *opaque) {
    struct lish_slirp *state = opaque;
    struct lish_timer *timer = timer_pointer;
    pthread_mutex_lock(&state->mutex);
    struct lish_timer **link = &state->timers;
    while (*link != NULL && *link != timer) {
        link = &(*link)->next;
    }
    if (*link == timer) {
        *link = timer->next;
    }
    pthread_mutex_unlock(&state->mutex);
    free(timer);
}

static void timer_mod(void *timer_pointer, int64_t expires_ms, void *opaque) {
    struct lish_slirp *state = opaque;
    struct lish_timer *timer = timer_pointer;
    pthread_mutex_lock(&state->mutex);
    timer->expires_ms = expires_ms;
    timer->armed = true;
    pthread_mutex_unlock(&state->mutex);
    wake_worker(state);
}

static void notify(void *opaque) {
    wake_worker(opaque);
}

static void register_poll_socket(slirp_os_socket socket, void *opaque) {
    (void)opaque;
#ifdef __APPLE__
    int enabled = 1;
    setsockopt(socket, SOL_SOCKET, SO_NOSIGPIPE, &enabled, sizeof(enabled));
#else
    (void)socket;
#endif
}

static void unregister_poll_socket(slirp_os_socket socket, void *opaque) {
    (void)socket;
    (void)opaque;
}

static int add_poll_socket(slirp_os_socket socket, int events, void *opaque) {
    struct poll_context *context = opaque;
    if (context->count == context->capacity) {
        size_t capacity = context->capacity == 0 ? 16 : context->capacity * 2;
        struct pollfd *fds = realloc(context->fds, capacity * sizeof(*fds));
        if (fds == NULL) {
            return -1;
        }
        context->fds = fds;
        context->capacity = capacity;
    }
    short poll_events = 0;
    if ((events & SLIRP_POLL_IN) != 0) poll_events |= POLLIN;
    if ((events & SLIRP_POLL_OUT) != 0) poll_events |= POLLOUT;
    if ((events & SLIRP_POLL_PRI) != 0) poll_events |= POLLPRI;
    context->fds[context->count] = (struct pollfd){
        .fd = socket,
        .events = poll_events,
        .revents = 0,
    };
    return (int)context->count++;
}

static int get_revents(int index, void *opaque) {
    struct poll_context *context = opaque;
    if (index < 0 || (size_t)index >= context->count) {
        return SLIRP_POLL_ERR;
    }
    short revents = context->fds[index].revents;
    int events = 0;
    if ((revents & POLLIN) != 0) events |= SLIRP_POLL_IN;
    if ((revents & POLLOUT) != 0) events |= SLIRP_POLL_OUT;
    if ((revents & POLLPRI) != 0) events |= SLIRP_POLL_PRI;
    if ((revents & POLLERR) != 0) events |= SLIRP_POLL_ERR;
    if ((revents & POLLHUP) != 0) events |= SLIRP_POLL_HUP;
    return events;
}

static int timer_timeout_ms(struct lish_slirp *state, uint32_t slirp_timeout) {
    int64_t now = monotonic_ns() / 1000000ll;
    int64_t timeout = slirp_timeout == UINT32_MAX ? INT32_MAX : slirp_timeout;
    pthread_mutex_lock(&state->mutex);
    for (struct lish_timer *timer = state->timers; timer != NULL; timer = timer->next) {
        if (timer->armed) {
            int64_t remaining = timer->expires_ms - now;
            if (remaining < timeout) timeout = remaining;
        }
    }
    pthread_mutex_unlock(&state->mutex);
    if (timeout < 0) return 0;
    if (timeout > INT32_MAX) return INT32_MAX;
    return (int)timeout;
}

static void fire_due_timers(struct lish_slirp *state) {
    for (;;) {
        SlirpTimerCb callback = NULL;
        void *callback_opaque = NULL;
        int64_t now = monotonic_ns() / 1000000ll;
        pthread_mutex_lock(&state->mutex);
        for (struct lish_timer *timer = state->timers; timer != NULL; timer = timer->next) {
            if (timer->armed && timer->expires_ms <= now) {
                timer->armed = false;
                callback = timer->callback;
                callback_opaque = timer->callback_opaque;
                break;
            }
        }
        pthread_mutex_unlock(&state->mutex);
        if (callback == NULL) return;
        callback(callback_opaque);
    }
}

static bool should_stop(struct lish_slirp *state) {
    pthread_mutex_lock(&state->mutex);
    bool stopping = state->stopping;
    pthread_mutex_unlock(&state->mutex);
    return stopping;
}

static void process_input(struct lish_slirp *state) {
    struct frame frame;
    for (;;) {
        pthread_mutex_lock(&state->mutex);
        bool available = queue_pop(&state->input, &frame);
        pthread_mutex_unlock(&state->mutex);
        if (!available) return;
        slirp_input(state->slirp, frame.bytes, (int)frame.length);
    }
}

static void process_command(struct lish_slirp *state) {
    pthread_mutex_lock(&state->mutex);
    struct forward_command command = state->command;
    pthread_mutex_unlock(&state->mutex);
    if (command.kind == COMMAND_NONE || command.complete) return;

    int result;
    if (command.kind == COMMAND_ADD_FORWARD) {
        result = slirp_add_hostfwd(state->slirp, command.udp,
                                   command.host_address, command.host_port,
                                   command.guest_address, command.guest_port);
    } else {
        result = slirp_remove_hostfwd(state->slirp, command.udp,
                                      command.host_address, command.host_port);
    }

    pthread_mutex_lock(&state->mutex);
    state->command.result = result;
    state->command.complete = true;
    pthread_cond_broadcast(&state->command_condition);
    pthread_mutex_unlock(&state->mutex);
}

static void *run_slirp(void *opaque) {
    struct lish_slirp *state = opaque;
    struct poll_context context = {0};
    while (!should_stop(state)) {
        process_input(state);
        process_command(state);
        fire_due_timers(state);

        context.count = 0;
        add_poll_socket(state->wake_read, SLIRP_POLL_IN, &context);
        uint32_t timeout = UINT32_MAX;
        slirp_pollfds_fill_socket(state->slirp, &timeout, add_poll_socket, &context);
        int result = poll(context.fds, (nfds_t)context.count, timer_timeout_ms(state, timeout));
        if (result > 0 && (context.fds[0].revents & POLLIN) != 0) {
            drain_wake_pipe(state);
        }
        int poll_error = result < 0 && errno != EINTR;
        slirp_pollfds_poll(state->slirp, poll_error, get_revents, &context);
    }
    free(context.fds);
    return NULL;
}

static bool set_nonblocking(int fd) {
    int flags = fcntl(fd, F_GETFL, 0);
    return flags >= 0 && fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}

lish_slirp_t *lish_slirp_create(const lish_slirp_config_t *input_config,
                                lish_slirp_output_ready_cb output_ready,
                                void *output_opaque,
                                char *error,
                                size_t error_capacity) {
    lish_slirp_config_t config = {
        .queue_capacity = LISH_DEFAULT_QUEUE_CAPACITY,
        .disable_host_loopback = true,
    };
    if (input_config != NULL) config = *input_config;
    if (config.queue_capacity == 0) config.queue_capacity = LISH_DEFAULT_QUEUE_CAPACITY;

    struct lish_slirp *state = calloc(1, sizeof(*state));
    if (state == NULL) {
        set_error(error, error_capacity, "unable to allocate libslirp state");
        return NULL;
    }
    state->wake_read = -1;
    state->wake_write = -1;
    state->output_ready = output_ready;
    state->output_opaque = output_opaque;
    pthread_mutex_init(&state->mutex, NULL);
    pthread_cond_init(&state->command_condition, NULL);
    pthread_cond_init(&state->output_condition, NULL);

    int pipe_fds[2] = {-1, -1};
    if (!queue_init(&state->input, config.queue_capacity) ||
        !queue_init(&state->output, config.queue_capacity) ||
        pipe(pipe_fds) != 0 || !set_nonblocking(pipe_fds[0]) || !set_nonblocking(pipe_fds[1])) {
        set_error(error, error_capacity, "unable to allocate bounded frame queues");
        goto fail;
    }
    state->wake_read = pipe_fds[0];
    state->wake_write = pipe_fds[1];
    pipe_fds[0] = -1;
    pipe_fds[1] = -1;

    state->config.version = SLIRP_CONFIG_VERSION_MAX;
    state->config.restricted = 0;
    state->config.in_enabled = true;
    inet_pton(AF_INET, "10.0.2.0", &state->config.vnetwork);
    inet_pton(AF_INET, "255.255.255.0", &state->config.vnetmask);
    inet_pton(AF_INET, "10.0.2.2", &state->config.vhost);
    inet_pton(AF_INET, "10.0.2.15", &state->config.vdhcp_start);
    inet_pton(AF_INET, "10.0.2.3", &state->config.vnameserver);
    state->config.in6_enabled = false;
    state->config.if_mtu = LISH_MTU;
    state->config.if_mru = LISH_MTU;
    state->config.disable_host_loopback = config.disable_host_loopback;
    state->config.disable_dns = false;
    state->config.disable_dhcp = false;

    state->callbacks.send_packet = send_packet;
    state->callbacks.guest_error = guest_error;
    state->callbacks.clock_get_ns = clock_get_ns;
    state->callbacks.timer_new = timer_new;
    state->callbacks.timer_free = timer_free;
    state->callbacks.timer_mod = timer_mod;
    state->callbacks.notify = notify;
    state->callbacks.register_poll_socket = register_poll_socket;
    state->callbacks.unregister_poll_socket = unregister_poll_socket;

    state->slirp = slirp_new(&state->config, &state->callbacks, state);
    if (state->slirp == NULL) {
        set_error(error, error_capacity, "libslirp rejected the network configuration");
        goto fail;
    }
    if (pthread_create(&state->thread, NULL, run_slirp, state) != 0) {
        set_error(error, error_capacity, "unable to create the libslirp poll thread");
        goto fail;
    }
    return state;

fail:
    if (state->slirp != NULL) slirp_cleanup(state->slirp);
    if (pipe_fds[0] >= 0) close(pipe_fds[0]);
    if (pipe_fds[1] >= 0) close(pipe_fds[1]);
    if (state->wake_read >= 0) close(state->wake_read);
    if (state->wake_write >= 0) close(state->wake_write);
    free(state->input.frames);
    free(state->output.frames);
    pthread_cond_destroy(&state->output_condition);
    pthread_cond_destroy(&state->command_condition);
    pthread_mutex_destroy(&state->mutex);
    free(state);
    return NULL;
}

int lish_slirp_input(lish_slirp_t *state, const uint8_t *frame, size_t length) {
    if (state == NULL || frame == NULL || length == 0 || length > LISH_MAX_FRAME_SIZE) {
        return -1;
    }
    pthread_mutex_lock(&state->mutex);
    bool accepted = !state->stopping && queue_push(&state->input, frame, length);
    if (accepted) {
        state->stats.frames_from_guest++;
        state->stats.bytes_from_guest += length;
    } else {
        state->stats.drops_from_guest++;
    }
    pthread_mutex_unlock(&state->mutex);
    if (accepted) wake_worker(state);
    return accepted ? 1 : 0;
}

int lish_slirp_next_output(lish_slirp_t *state,
                           uint8_t *frame,
                           size_t capacity,
                           size_t *length) {
    if (state == NULL || frame == NULL || length == NULL) return -1;
    pthread_mutex_lock(&state->mutex);
    if (state->output.count == 0) {
        pthread_mutex_unlock(&state->mutex);
        return 0;
    }
    struct frame *next = &state->output.frames[state->output.head];
    if (next->length > capacity) {
        *length = next->length;
        pthread_mutex_unlock(&state->mutex);
        return -1;
    }
    struct frame value;
    queue_pop(&state->output, &value);
    pthread_cond_signal(&state->output_condition);
    pthread_mutex_unlock(&state->mutex);
    memcpy(frame, value.bytes, value.length);
    *length = value.length;
    return 1;
}

static int execute_forward_command(struct lish_slirp *state,
                                   struct forward_command command) {
    if (state == NULL) return -1;
    pthread_mutex_lock(&state->mutex);
    while (state->command.kind != COMMAND_NONE && !state->stopping) {
        pthread_cond_wait(&state->command_condition, &state->mutex);
    }
    if (state->stopping) {
        pthread_mutex_unlock(&state->mutex);
        return -1;
    }
    state->command = command;
    pthread_mutex_unlock(&state->mutex);
    wake_worker(state);

    pthread_mutex_lock(&state->mutex);
    while (!state->command.complete && !state->stopping) {
        pthread_cond_wait(&state->command_condition, &state->mutex);
    }
    int result = state->stopping ? -1 : state->command.result;
    state->command = (struct forward_command){0};
    pthread_cond_broadcast(&state->command_condition);
    pthread_mutex_unlock(&state->mutex);
    return result;
}

int lish_slirp_add_host_forward(lish_slirp_t *state,
                                bool udp,
                                const char *host_address,
                                uint16_t host_port,
                                const char *guest_address,
                                uint16_t guest_port) {
    struct forward_command command = {
        .kind = COMMAND_ADD_FORWARD,
        .udp = udp,
        .host_port = host_port,
        .guest_port = guest_port,
    };
    if (host_address == NULL || guest_address == NULL ||
        inet_pton(AF_INET, host_address, &command.host_address) != 1 ||
        inet_pton(AF_INET, guest_address, &command.guest_address) != 1) {
        return -1;
    }
    return execute_forward_command(state, command);
}

int lish_slirp_remove_host_forward(lish_slirp_t *state,
                                   bool udp,
                                   const char *host_address,
                                   uint16_t host_port) {
    struct forward_command command = {
        .kind = COMMAND_REMOVE_FORWARD,
        .udp = udp,
        .host_port = host_port,
    };
    if (host_address == NULL ||
        inet_pton(AF_INET, host_address, &command.host_address) != 1) {
        return -1;
    }
    return execute_forward_command(state, command);
}

void lish_slirp_get_stats(lish_slirp_t *state, lish_slirp_stats_t *stats) {
    if (state == NULL || stats == NULL) return;
    pthread_mutex_lock(&state->mutex);
    *stats = state->stats;
    stats->queued_from_guest = state->input.count;
    stats->queued_to_guest = state->output.count;
    pthread_mutex_unlock(&state->mutex);
}

void lish_slirp_stop(lish_slirp_t *state) {
    if (state == NULL) return;
    pthread_mutex_lock(&state->mutex);
    state->stopping = true;
    pthread_cond_broadcast(&state->command_condition);
    pthread_cond_broadcast(&state->output_condition);
    pthread_mutex_unlock(&state->mutex);
    wake_worker(state);
}

void lish_slirp_destroy(lish_slirp_t *state) {
    if (state == NULL) return;
    lish_slirp_stop(state);
    pthread_join(state->thread, NULL);
    slirp_cleanup(state->slirp);

    struct lish_timer *timer = state->timers;
    while (timer != NULL) {
        struct lish_timer *next = timer->next;
        free(timer);
        timer = next;
    }
    close(state->wake_read);
    close(state->wake_write);
    free(state->input.frames);
    free(state->output.frames);
    pthread_cond_destroy(&state->output_condition);
    pthread_cond_destroy(&state->command_condition);
    pthread_mutex_destroy(&state->mutex);
    free(state);
}
