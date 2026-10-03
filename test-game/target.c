/* Stand-in "game": keeps gold and hp (int), energy (float), shield (double)
 * and scrap (int XOR-encoded with a per-object key, like Infested Planet's BP)
 * on the heap, plus coins and wood (doubles in their own blocks, read through one
 * shared function like GameMaker games read every variable, and coins also directly),
 * plus gems and ore (doubles at the end of a chain from a static pointer: world -> room -> stats,
 * only ever read and written through shared functions, so only pointer paths find it again),
 * and changes them on commands (earn N, spend N, hit N, gain X, shield X, scrap N,
 * plus food (a 16-bit short, which Ferret doesn't search for) with a float copy of it for the HUD,
 * refreshed every tick (a search can only end on that copy, which Ferret must reject),
 * plus crystals (an int in the base object, which the code reads from a static pointer, like
 * Particle Fleet's gems),
 * coins N, wood N, gems N, ore N, eat N, crystals N, show, respawn, newroom, newmap) dropped into a
 * command file, so it can be driven the same way natively, under Proton and inside the Steam
 * runtime. "respawn" moves the player to a new object and frees the old one,
 * like a new mission; "newroom" does the same with the room and its stats. "newmap" moves the
 * player and the base to new objects and keeps the old ones (garbage a collector hasn't freed
 * yet: they stay readable and look alive).
 * Usage: target <command file> <log file>. Built for Linux and Windows. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
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
    float energy;
    double shield;
    int scrap_enc;
    int scrap_key;
};

struct stats {
    const char *type;
    double seen[3];
    double gems;
    double ore;
};

struct room {
    const char *type;
    char name[48];
    struct stats *stats;
};

struct world {
    const char *type;
    int ticks;
    struct room *room;
};

static struct world *world;

static const char base_type[] = "base";

struct base {
    const char *type;
    int level;
    int crystals;
};

static struct base *base;

static struct base *new_base(int crystals)
{
    struct base *b = malloc(sizeof *b);

    b->type = base_type;
    b->level = 1;
    b->crystals = crystals;
    return b;
}

static int scrap(const struct player *p)
{
    return p->scrap_enc ^ p->scrap_key;
}

static void add_scrap(struct player *p, int n)
{
    p->scrap_enc = (scrap(p) + n) ^ p->scrap_key;
}

static struct player *spawn(void)
{
    struct player *p = malloc(sizeof *p);

    p->type = player_type;
    p->hp = 100;
    p->gold = 1000;
    p->energy = 1.5f;
    p->shield = 250.25;
    p->scrap_key = (rand() << 16 ^ rand()) | 1;
    p->scrap_enc = 50 ^ p->scrap_key;
    return p;
}

/* One function reads every number, so its code touches many addresses. */
__attribute__((noinline, noipa)) static double read_real(const double *v)
{
    return *v;
}

__attribute__((noinline, noipa)) static void write_real(double *v, double x)
{
    *v = x;
}

static struct room *new_room(double gems, double ore)
{
    struct room *r = malloc(sizeof *r);

    r->type = "room";
    strcpy(r->name, "cave");
    r->stats = malloc(sizeof *r->stats);
    r->stats->type = "stats";
    write_real(&r->stats->gems, gems);
    write_real(&r->stats->ore, ore);
    return r;
}

/* Food and a neighbour, so the pair doesn't read as an int holding food. */
static struct {
    short food;
    short thirst;
} needs = { 500, 999 };

static void report(const char *log, const struct player *p, const double *coins, const double *wood)
{
    FILE *f = fopen(log, "a");

    if (f) {
        fprintf(f, "gold=%d hp=%d energy=%.2f shield=%.2f scrap=%d coins=%.0f wood=%.0f gems=%.0f ore=%.0f food=%d crystals=%d\n",
                p->gold, p->hp, p->energy, p->shield, scrap(p), *coins, *wood, read_real(&world->room->stats->gems),
                read_real(&world->room->stats->ore), needs.food, base->crystals);
        fclose(f);
    }
}

int main(int argc, char **argv)
{
    char *padding = malloc(123456);
    struct player *p = spawn();
    double *coins = malloc(sizeof *coins);
    double *wood = malloc(sizeof *wood);
    volatile float *hud_food = malloc(sizeof *hud_food);
    struct player *old;
    volatile int richest = 0;
    volatile float most_energy = 0;
    volatile double most_shield = 0;
    volatile int most_scrap = 0;
    volatile int most_crystals = 0;
    volatile double most_coins = 0, total = 0;
    float x;
    char line[64];
    FILE *f;
    int n;

    if (argc < 3)
        return 1;
    srand((unsigned)time(NULL));
    padding[0] = 1;
    *coins = 30;
    *wood = 12;
    world = malloc(sizeof *world);
    world->type = "world";
    world->room = new_room(77, 40);
    base = new_base(300);
    report(argv[2], p, coins, wood);
    for (;;) {
        sleep_ms(100);
        *hud_food = needs.food;
        if (p->gold > richest)
            richest = p->gold;
        if (p->energy > most_energy)
            most_energy = p->energy;
        if (p->shield > most_shield)
            most_shield = p->shield;
        if (base->crystals > most_crystals)
            most_crystals = base->crystals;
        if (scrap(p) > most_scrap)
            most_scrap = scrap(p);
        total = read_real(coins) + read_real(wood) + read_real(&world->room->stats->gems);
        world->ticks++;
        if (*coins > most_coins)
            most_coins = *coins;
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
        else if (sscanf(line, "gain %f", &x) == 1)
            p->energy += x;
        else if (sscanf(line, "shield %f", &x) == 1)
            p->shield += x;
        else if (sscanf(line, "scrap %d", &n) == 1)
            add_scrap(p, n);
        else if (sscanf(line, "coins %d", &n) == 1)
            *coins += n;
        else if (sscanf(line, "wood %d", &n) == 1)
            *wood += n;
        else if (sscanf(line, "eat %d", &n) == 1)
            needs.food += n;
        else if (sscanf(line, "gems %d", &n) == 1)
            write_real(&world->room->stats->gems, read_real(&world->room->stats->gems) + n);
        else if (sscanf(line, "ore %d", &n) == 1)
            write_real(&world->room->stats->ore, read_real(&world->room->stats->ore) + n);
        else if (sscanf(line, "crystals %d", &n) == 1)
            base->crystals += n;
        else if (strncmp(line, "newmap", 6) == 0) {
            struct player *q = spawn();

            q->gold = p->gold;
            p = q;
            base = new_base(base->crystals);
        } else if (strncmp(line, "newroom", 7) == 0) {
            struct room *r = world->room;

            padding = malloc(4096);
            world->room = new_room(read_real(&r->stats->gems), read_real(&r->stats->ore));
            free(r->stats);
            free(r);
        } else if (sscanf(line, "quit %d", &n) == 1)
            break;
        else if (strncmp(line, "respawn", 7) == 0) {
            old = p;
            padding = malloc(4096);
            p = spawn();
            free(old);
        }
        report(argv[2], p, coins, wood);
    }
    return 0;
}
