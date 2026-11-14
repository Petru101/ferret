namespace Standin;

/// Numbers no object holds: static fields.
public static class GameState
{
    public static int Coins = 30;
    public static float Difficulty = 1.25f;
    public const int MaxCoins = 9999;
}

/// A singleton: a plain object only its own static field leads to.
public class Run
{
    public static Run Instance = new Run();
    public int Wave = 3;
    public double Timer = 99.5;
}
