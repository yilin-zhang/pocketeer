# Cores, BIOS and cleanup

```sh
pocketeer status                                 # card, firmware, disk, core counts
pocketeer cores list [--outdated]
pocketeer cores search gba
pocketeer cores info jotego.jtcps1
pocketeer cores install jotego.jtcps1            # also name@version, or --from core.zip
pocketeer cores update --all
pocketeer cores remove jotego.jtcps1             # --purge removes settings too
pocketeer inventory refresh
```

```sh
pocketeer bios check                             # what cores need, and what's missing
pocketeer bios place                             # files dropped at the SD root → where they belong
pocketeer clean                                  # ._* / .DS_Store / __MACOSX
```
