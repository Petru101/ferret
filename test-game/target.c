/* Stand-in "game": keeps gold and hp on the heap and changes them on commands
 * (earn N, spend N, hit N, show, respawn) dropped into a command file, so it
 * can be driven the same way natively, under Proton and inside the Steam
 * runtime. "respawn" moves the player to a new object and frees the old one,
 * like a new mission. Usage: target <command file> <log file>. Built for Linux
 * and Windows. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
#define sleep_ms(ms) Sleep(ms)
#else
#include <unistd.h>
#define sleep_ms(ms) usleep((ms) * 1000)
#endif

static const char player_type[] = "player";

struct player {
    const char *type;
    int hp;
    int gold;
};

static struct player *spawn(void)
{
    struct player *p = malloc(sizeof *p);

    p->type = player_type;
    p->hp = 100;
    p->gold = 1000;
    return p;
}

static void report(const char *log, const struct player *p)
{
    FILE *f = fopen(log, "a");

    if (f) {
        fprintf(f, "gold=%d hp=%d\n", p->gold, p->hp);
        fclose(f);
    }
}

int main(int argc, char **argv)
{
    char *padding = malloc(123456);
    struct player *p = spawn();
    struct player *old;
    volatile int richest = 0;
    char line[64];
    FILE *f;
    int n;

    if (argc < 3)
        return 1;
    padding[0] = 1;
    report(argv[2], p);
    for (;;) {
        sleep_ms(100);
        if (p->gold > richest)
            richest = p->gold;
        f = fopen(argv[1], "r");
        if (!f)
            continue;
        if (!fgets(line, sizeof line, f))
            line[0] = 0;
        fclose(f);
        remove(argv[1]);
        if (sscanf(line, "earn %d", &n) == 1)
            p->gold += n;
        else if (sscanf(line, "spend %d", &n) == 1)
            p->gold -= n;
        else if (sscanf(line, "hit %d", &n) == 1)
            p->hp -= n;
        else if (sscanf(line, "quit %d", &n) == 1)
            break;
        else if (strncmp(line, "respawn", 7) == 0) {
            old = p;
            padding = malloc(4096);
            p = spawn();
            free(old);
        }
        report(argv[2], p);
    }
    return 0;
}
