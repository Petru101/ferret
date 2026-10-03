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
 * Plus a Unity (Mono) style inventory, laid out like Valheim's: managed objects (a two-pointer
 * header), UTF-16 strings and arrays, reached only from objects allocated at run time:
 * "Inventory" -> list -> array of items -> item -> its info -> "$item_logs", the stack count an
 * int in each item. "logs N" adds to the first logs stack, "stack N" adds a logs stack, "stone N"
 * a stone stack, "chest N" logs in a chest (an inventory named "$piece_chest"), "die" moves the
 * stacks into a new "Inventory" (the player object points at it) and keeps the old, emptied one
 * (garbage). A label object also
 * holds a string "Inventory" (a root that leads nowhere).
 * Usage: target <command file> <log file>. Built for Linux and Windows. */
#include <stdint.h>
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

/* Managed objects: a type pointer and a lock word first. */
struct header {
    void *vtable;
    void *sync;
};

struct string {
    struct header h;
    int32_t length;
    uint16_t chars[];
};

struct array {
    struct header h;
    void *bounds;
    uintptr_t length;
    void *items[];
};

struct item_info {
    struct header h;
    struct string *name;
    struct string *description;
    int max_stack;
};

struct item {
    struct header h;
    struct item_info *info;
    void *drop_prefab;
    struct string *crafter;
    void *custom;
    int stack;
    float durability;
};

struct item_list {
    struct header h;
    struct array *items;
    int size;
    int version;
};

struct inventory {
    struct header h;
    struct string *name;
    void *background;
    struct item_list *list;
    int width, height;
};

struct humanoid {
    struct header h;
    struct string *name;
    struct inventory *inventory;
    float health;
};

struct label {
    struct header h;
    struct string *text;
    int font_size;
};

/* Type objects live in the managed heap too. */
static void *new_type(void)
{
    return calloc(1, 64);
}

static void *string_type, *array_type, *info_type, *item_type, *list_type, *inventory_type, *label_type, *humanoid_type;

static struct string *new_string(const char *text)
{
    size_t n = strlen(text), i;
    struct string *s = calloc(1, sizeof *s + 2 * n + 2);

    s->h.vtable = string_type;
    s->length = (int32_t)n;
    for (i = 0; i < n; i++)
        s->chars[i] = (uint16_t)text[i];
    return s;
}

static struct item_info *logs_info, *stone_info;

static struct item_info *new_info(const char *name, const char *description)
{
    struct item_info *t = calloc(1, sizeof *t);

    t->h.vtable = info_type;
    t->name = new_string(name);
    t->description = new_string(description);
    t->max_stack = 50;
    return t;
}

static struct inventory *new_inventory(const char *name)
{
    struct inventory *inv = calloc(1, sizeof *inv);

    inv->h.vtable = inventory_type;
    inv->name = new_string(name);
    inv->list = calloc(1, sizeof *inv->list);
    inv->list->h.vtable = list_type;
    inv->list->items = calloc(1, sizeof *inv->list->items + 32 * sizeof(void *));
    inv->list->items->h.vtable = array_type;
    inv->list->items->length = 32;
    inv->width = 8;
    inv->height = 4;
    return inv;
}

static void add_stack(struct inventory *inv, struct item_info *info, int n)
{
    struct item *it = calloc(1, sizeof *it);

    it->h.vtable = item_type;
    it->info = info;
    it->crafter = new_string("");
    it->stack = n;
    it->durability = 100;
    inv->list->items->items[inv->list->size++] = it;
}

static struct item *first_stack(const struct inventory *inv, const struct item_info *info)
{
    int i;

    for (i = 0; i < inv->list->size; i++) {
        struct item *it = inv->list->items->items[i];
        if (it->info == info)
            return it;
    }
    return NULL;
}

static void print_stacks(FILE *f, const char *what, const struct inventory *inv, const struct item_info *info)
{
    int i, any = 0;

    fprintf(f, " %s=", what);
    for (i = 0; i < inv->list->size; i++) {
        const struct item *it = inv->list->items->items[i];
        if (it->info == info)
            fprintf(f, "%s%d", any++ ? "," : "", it->stack);
    }
    if (!any)
        fprintf(f, "-");
}

/* Food and a neighbour, so the pair doesn't read as an int holding food. */
static struct {
    short food;
    short thirst;
} needs = { 500, 999 };

static void report(const char *log, const struct player *p, const double *coins, const double *wood, const struct inventory *inv,
                   const struct inventory *chest)
{
    FILE *f = fopen(log, "a");

    if (f) {
        fprintf(f, "gold=%d hp=%d energy=%.2f shield=%.2f scrap=%d coins=%.0f wood=%.0f gems=%.0f ore=%.0f food=%d crystals=%d",
                p->gold, p->hp, p->energy, p->shield, scrap(p), *coins, *wood, read_real(&world->room->stats->gems),
                read_real(&world->room->stats->ore), needs.food, base->crystals);
        print_stacks(f, "logs", inv, logs_info);
        print_stacks(f, "stone", inv, stone_info);
        print_stacks(f, "chestlogs", chest, logs_info);
        fprintf(f, "\n");
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
    struct inventory *inv, *chest;
    struct label *label;
    struct humanoid *hero;
    struct item *it;
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
    string_type = new_type();
    array_type = new_type();
    info_type = new_type();
    item_type = new_type();
    list_type = new_type();
    inventory_type = new_type();
    label_type = new_type();
    humanoid_type = new_type();
    logs_info = new_info("$item_logs", "$item_logs_description");
    stone_info = new_info("$item_stone", "$item_stone_description");
    label = calloc(1, sizeof *label);
    label->h.vtable = label_type;
    label->text = new_string("Inventory");
    label->font_size = 14;
    inv = new_inventory("Inventory");
    add_stack(inv, stone_info, 9);
    add_stack(inv, logs_info, 33);
    hero = calloc(1, sizeof *hero);
    hero->h.vtable = humanoid_type;
    hero->name = new_string("Player");
    hero->inventory = inv;
    hero->health = 25;
    chest = new_inventory("$piece_chest");
    chest->width = 5;
    chest->height = 2;
    add_stack(chest, logs_info, 50);
    report(argv[2], p, coins, wood, inv, chest);
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
        else if (sscanf(line, "logs %d", &n) == 1) {
            if ((it = first_stack(inv, logs_info)))
                it->stack += n;
        } else if (sscanf(line, "stack %d", &n) == 1)
            add_stack(inv, logs_info, n);
        else if (sscanf(line, "stone %d", &n) == 1)
            add_stack(inv, stone_info, n);
        else if (sscanf(line, "chest %d", &n) == 1) {
            if ((it = first_stack(chest, logs_info)))
                it->stack += n;
        } else if (strncmp(line, "die", 3) == 0) {
            struct inventory *next = new_inventory("Inventory");
            int i;

            for (i = 0; i < inv->list->size; i++) {
                next->list->items->items[i] = inv->list->items->items[i];
                inv->list->items->items[i] = NULL;
            }
            next->list->size = inv->list->size;
            inv->list->size = 0;
            inv = next;
            hero->inventory = inv;
        }
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
        report(argv[2], p, coins, wood, inv, chest);
    }
    return 0;
}
