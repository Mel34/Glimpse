#include <libavformat/avformat.h>
#include <libavutil/log.h>

#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define UDTA_WARNING "UDTA parsing failed retrying raw"

struct seen_file {
    char *path;
    struct seen_file *next;
};

static pthread_mutex_t seen_files_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t install_once = PTHREAD_ONCE_INIT;
static struct seen_file *seen_files;

static int should_forward_udta_warning(void *avcl)
{
    const AVClass *av_class;
    const AVFormatContext *format;
    const char *path;
    struct seen_file *entry;

    if (avcl == NULL) {
        return 1;
    }

    av_class = *(const AVClass * const *)avcl;
    if (av_class != avformat_get_class()) {
        return 1;
    }

    format = (const AVFormatContext *)avcl;
    path = format->url;
    if (path == NULL || path[0] == '\0') {
        return 1;
    }

    pthread_mutex_lock(&seen_files_mutex);
    for (entry = seen_files; entry != NULL; entry = entry->next) {
        if (strcmp(entry->path, path) == 0) {
            pthread_mutex_unlock(&seen_files_mutex);
            return 0;
        }
    }

    entry = (struct seen_file *)malloc(sizeof(*entry));
    if (entry == NULL) {
        pthread_mutex_unlock(&seen_files_mutex);
        return 1;
    }

    entry->path = (char *)malloc(strlen(path) + 1);
    if (entry->path == NULL) {
        free(entry);
        pthread_mutex_unlock(&seen_files_mutex);
        return 1;
    }

    strcpy(entry->path, path);
    entry->next = seen_files;
    seen_files = entry;
    pthread_mutex_unlock(&seen_files_mutex);
    return 1;
}

static void glimpse_ffmpeg_log_callback(void *avcl, int level,
                                        const char *format, va_list arguments)
{
    char message[2048];
    va_list copy;

    va_copy(copy, arguments);
    vsnprintf(message, sizeof(message), format, copy);
    va_end(copy);

    if (strstr(message, UDTA_WARNING) != NULL
        && !should_forward_udta_warning(avcl)) {
        return;
    }

    av_log_default_callback(avcl, level, format, arguments);
}

static void install_log_callback(void)
{
    av_log_set_callback(glimpse_ffmpeg_log_callback);
}

void glimpse_ffmpeg_install_log_filter(void)
{
    pthread_once(&install_once, install_log_callback);
}
