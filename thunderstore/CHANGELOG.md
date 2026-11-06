# 1.0.1

- Fix: Delete the merge scratch folder (`fusion/work`) after merge finishes. Its extracted paks were deep enough that any Lua mod calling `IterateGameDirectories` could crash the game on UE4SS releases without the fix from [RE-UE4SS#1374](https://github.com/UE4SS-RE/RE-UE4SS/pull/1374).

# 1.0.0

Initial release