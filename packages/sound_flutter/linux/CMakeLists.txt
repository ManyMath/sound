# Builds the shared `sound_cli` Rust crate via cargokit and bundles the
# resulting libsound_cli.so with the Flutter app.
cmake_minimum_required(VERSION 3.10)

set(PROJECT_NAME "sound_flutter")
project(${PROJECT_NAME} LANGUAGES CXX)

include("${CMAKE_CURRENT_SOURCE_DIR}/../cargokit/cmake/cargokit.cmake")

# apply_cargokit(<target> <manifest_dir, relative to this file> <lib_name> <force-link symbol>)
# The crate lives in ../rust so it ships with the published package.
apply_cargokit(${PROJECT_NAME} ../rust sound_cli "")

set(sound_flutter_bundled_libraries
  "${${PROJECT_NAME}_cargokit_lib}"
  PARENT_SCOPE
)
