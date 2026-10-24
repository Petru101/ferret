// Java stand-in: the game's state hangs off a Runnable its game thread runs (no static field
// leads to it, like Blue Boy Adventure's GamePanel), with a long, a byte and a boolean on the
// player and items in every kind of map Ferret looks through. Run: java Stand.java <cmd file> <log file>
// Commands (one per line, the file is removed once read): gold N (adds), hurt N, level N,
// potion N (adds to items["Health Potion"]), ore N (adds to the TreeMap's), str N (adds to the
// EnumMap's), slot N (adds to the ConcurrentHashMap's slot 7), grow (300 more map entries: the
// tables grow, nodes move), gc (the collector moves objects), show, quit.
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

public class Stand {
    static long bestScore = 12345678901L;
    static final int MAX_HP = 100;

    enum Stat { STRENGTH, AGILITY }

    static class Item {
        int count;
        Item(int c) { count = c; }
    }

    static class Player {
        int hp = 100;
        long gold = 5_000_000_000L;
        byte level = 3;
        boolean invincible = false;
    }

    static class Game implements Runnable {
        final Path cmd, log;
        Player player = new Player();
        HashMap<String, Item> items = new HashMap<>();
        TreeMap<String, Item> ores = new TreeMap<>();
        EnumMap<Stat, Item> stats = new EnumMap<>(Stat.class);
        ConcurrentHashMap<Integer, Item> slots = new ConcurrentHashMap<>();
        HashMap<String, Integer> boxed = new HashMap<>();

        Game(Path cmd, Path log) {
            this.cmd = cmd;
            this.log = log;
            items.put("Health Potion", new Item(2));
            items.put("Key", new Item(1));
            ores.put("iron", new Item(40));
            ores.put("gold", new Item(4));
            stats.put(Stat.STRENGTH, new Item(10));
            stats.put(Stat.AGILITY, new Item(7));
            slots.put(7, new Item(70));
            boxed.put("lives", 3);
        }

        void show() throws Exception {
            String line = String.format("hp=%d gold=%d level=%d invincible=%b potions=%d ore=%d str=%d slot=%d%n", player.hp, player.gold, player.level, player.invincible,
                    items.get("Health Potion").count, ores.get("iron").count, stats.get(Stat.STRENGTH).count, slots.get(7).count);
            Files.writeString(log, line, StandardOpenOption.CREATE, StandardOpenOption.APPEND);
        }

        public void run() {
            List<Object> garbage = new ArrayList<>();
            try {
                show();
                while (true) {
                    Thread.sleep(100);
                    if (!Files.exists(cmd)) continue;
                    List<String> lines = Files.readAllLines(cmd);
                    Files.delete(cmd);
                    for (String l : lines) {
                        String[] w = l.trim().split(" ");
                        long big = w.length > 1 ? Long.parseLong(w[1]) : 0;
                        int n = (int) big;
                        switch (w[0]) {
                            case "gold" -> player.gold += big;
                            case "hurt" -> { if (!player.invincible) player.hp = Math.max(0, player.hp - n); }
                            case "level" -> player.level = (byte) n;
                            case "potion" -> items.get("Health Potion").count += n;
                            case "ore" -> ores.get("iron").count += n;
                            case "str" -> stats.get(Stat.STRENGTH).count += n;
                            case "slot" -> slots.get(7).count += n;
                            case "grow" -> {
                                for (int i = 0; i < 300; i++) {
                                    items.put("junk" + items.size(), new Item(i));
                                    slots.put(100 + slots.size(), new Item(i));
                                }
                            }
                            case "gc" -> {
                                for (int i = 0; i < 200000; i++) garbage.add(new byte[64]);
                                garbage.clear();
                                System.gc();
                            }
                            case "quit" -> System.exit(n);
                        }
                    }
                    show();
                }
            } catch (Exception e) {
                e.printStackTrace();
                System.exit(1);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(new Game(Path.of(args[0]), Path.of(args[1])), "Game Loop");
        t.start();
        t.join();
    }
}
