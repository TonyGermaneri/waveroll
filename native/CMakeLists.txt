# Waveroll's native builds: AU, VST3, CLAP and a standalone app from one target.
#
# JUCE is here for one reason above all others -- it emits AU, and Logic loads nothing else. It
# also emits a standalone app, which is why there is no separate desktop shell in this project.
cmake_minimum_required(VERSION 3.22)
project(Waveroll VERSION 0.1.0)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
# Universal from the start: an Apple Silicon machine building x86-only plugins is a thing you
# discover when somebody else cannot load them.
set(CMAKE_OSX_ARCHITECTURES "arm64;x86_64" CACHE STRING "" FORCE)
# Set explicitly, because it must not be left to be inferred. Corrosion sets a deployment target
# for the Rust build, and with none set here that leaks into the C++ link -- where it makes the
# libc++abi entry points that newer libc++ headers call (__cxa_init_primary_exception, and
# std::exception_ptr::__from_native_exception_pointer) unavailable, and the link fails with a bare
# "symbol(s) not found" that names neither cause nor cure.
set(CMAKE_OSX_DEPLOYMENT_TARGET "13.0" CACHE STRING "" FORCE)

include(FetchContent)
FetchContent_Declare(JUCE
  GIT_REPOSITORY https://github.com/juce-framework/JUCE.git
  GIT_TAG 8.0.8
  GIT_SHALLOW TRUE
  GIT_PROGRESS TRUE)
FetchContent_MakeAvailable(JUCE)

# Corrosion builds the Rust core and hands CMake a target to link. It also handles the universal
# build: with two OSX architectures it compiles both Rust targets and lipos them, which is the
# thing that is tedious to do by hand and easy to get silently half-right.
FetchContent_Declare(Corrosion
  GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
  GIT_TAG v0.5.2
  GIT_SHALLOW TRUE)
FetchContent_MakeAvailable(Corrosion)

corrosion_import_crate(
  MANIFEST_PATH "${CMAKE_CURRENT_SOURCE_DIR}/../crates/waveroll-ffi/Cargo.toml"
  CRATES waveroll-ffi
  PROFILE release)

juce_add_plugin(Waveroll
  COMPANY_NAME "Waveroll"
  PLUGIN_MANUFACTURER_CODE Wvrl
  PLUGIN_CODE Wvr1
  FORMATS AU VST3 Standalone
  PRODUCT_NAME "Waveroll"
  IS_SYNTH FALSE
  NEEDS_MIDI_INPUT TRUE
  NEEDS_MIDI_OUTPUT FALSE
  IS_MIDI_EFFECT FALSE
  EDITOR_WANTS_KEYBOARD_FOCUS TRUE
  COPY_PLUGIN_AFTER_BUILD TRUE)

target_sources(Waveroll PRIVATE
  Source/PluginProcessor.cpp
  Source/PluginEditor.cpp)

target_include_directories(Waveroll PRIVATE include)

target_compile_definitions(Waveroll PUBLIC
  JUCE_WEB_BROWSER=0
  JUCE_USE_CURL=0
  JUCE_VST3_CAN_REPLACE_VST2=0
  JUCE_DISPLAY_SPLASH_SCREEN=0)

# The Rust staticlib comes last, and c++abi is named explicitly. A Rust staticlib brings its own
# copy of the unwinder along, and if it lands ahead of the C++ runtime on the link line the
# resolution of __cxa_init_primary_exception goes to the wrong one and fails with a bare
# "symbol(s) not found".
target_link_libraries(Waveroll PRIVATE
  juce::juce_audio_utils
  waveroll_ffi
  c++abi
  PUBLIC
  juce::juce_recommended_config_flags
  juce::juce_recommended_warning_flags)
