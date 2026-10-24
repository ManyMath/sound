# Local Cargokit patches

`packages/sound_flutter/cargokit/build_pod.sh`: remove the unconditional environment dump, which can expose CI secrets. Build behavior is otherwise unchanged.
