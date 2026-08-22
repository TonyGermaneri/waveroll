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

# The Rust core, built by hand rather than through Corrosion.
#
# Corrosion was here for exactly one reason -- the universal build -- and it does not do it: it
# builds a single Rust target and never looks at CMAKE_OSX_ARCHITECTURES, so the C++ came out
# universal and the Rust arm64-only, and the x86_64 link failed on every wr_ symbol. Twenty lines
# of cargo and lipo do the job with no dependency and no surprises.
set(WAVEROLL_RUST_TARGETS "")
set(WAVEROLL_RUST_SLICES "")
foreach(arch IN LISTS CMAKE_OSX_ARCHITECTURES)
  if(arch STREQUAL "arm64")
    set(triple aarch64-apple-darwin)
  elseif(arch STREQUAL "x86_64")
    set(triple x86_64-apple-darwin)
  else()
    message(FATAL_ERROR "no Rust triple known for architecture ${arch}")
  endif()
  list(APPEND WAVEROLL_RUST_TARGETS --target ${triple})
  list(APPEND WAVEROLL_RUST_SLICES
       "${CMAKE_CURRENT_SOURCE_DIR}/../target/${triple}/release/libwaveroll_ffi.a")
endforeach()

set(WAVEROLL_RUST_LIB "${CMAKE_CURRENT_BINARY_DIR}/libwaveroll_ffi.a")

# Always run: cargo is already incremental and decides for itself whether anything changed, which
# is a better judge than a DEPENDS list that goes stale the first time a file is added.
add_custom_target(waveroll_rust ALL
  COMMAND cargo build --release
          --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/../Cargo.toml"
          -p waveroll-ffi ${WAVEROLL_RUST_TARGETS}
  COMMAND lipo -create ${WAVEROLL_RUST_SLICES} -output "${WAVEROLL_RUST_LIB}"
  BYPRODUCTS "${WAVEROLL_RUST_LIB}"
  COMMENT "Building the Rust core for ${CMAKE_OSX_ARCHITECTURES}"
  VERBATIM)

add_library(waveroll_ffi INTERFACE)
target_link_libraries(waveroll_ffi INTERFACE "${WAVEROLL_RUST_LIB}")

# The stamp is regenerated on every build and lands in its own translation unit, so a changed
# stamp recompiles one small file rather than the editor.
set(WAVEROLL_STAMP "${CMAKE_CURRENT_BINARY_DIR}/BuildStamp.cpp")
add_custom_target(waveroll_stamp ALL
  COMMAND ${CMAKE_COMMAND}
          -DSOURCE_DIR=${CMAKE_CURRENT_SOURCE_DIR}
          -DOUT=${WAVEROLL_STAMP}
          -P ${CMAKE_CURRENT_SOURCE_DIR}/cmake/BuildStamp.cmake
  BYPRODUCTS ${WAVEROLL_STAMP}
  COMMENT "Stamping the build"
  VERBATIM)
# Generated before the first build, so the first configure has something to compile.
execute_process(COMMAND ${CMAKE_COMMAND}
  -DSOURCE_DIR=${CMAKE_CURRENT_SOURCE_DIR} -DOUT=${WAVEROLL_STAMP}
  -P ${CMAKE_CURRENT_SOURCE_DIR}/cmake/BuildStamp.cmake)

# The plugin's own version tracks the commit count, so Live's plugin info says which commit a
# binary came from rather than 0.1.0 forever.
find_package(Git QUIET)
set(WAVEROLL_BUILD 0)
if(GIT_FOUND)
  execute_process(COMMAND ${GIT_EXECUTABLE} rev-list --count HEAD
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR} OUTPUT_VARIABLE WAVEROLL_BUILD
    OUTPUT_STRIP_TRAILING_WHITESPACE ERROR_QUIET)
endif()

juce_add_plugin(Waveroll
  VERSION 0.1.${WAVEROLL_BUILD}
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
  Source/PluginEditor.cpp
  ${WAVEROLL_STAMP})

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
add_dependencies(Waveroll waveroll_rust waveroll_stamp)

target_link_libraries(Waveroll PRIVATE
  juce::juce_audio_utils
  waveroll_ffi
  c++abi
  PUBLIC
  juce::juce_recommended_config_flags
  juce::juce_recommended_warning_flags)
