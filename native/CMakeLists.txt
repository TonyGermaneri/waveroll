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

include(FetchContent)
FetchContent_Declare(JUCE
  GIT_REPOSITORY https://github.com/juce-framework/JUCE.git
  GIT_TAG 8.0.8
  GIT_SHALLOW TRUE
  GIT_PROGRESS TRUE)
FetchContent_MakeAvailable(JUCE)

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

target_compile_definitions(Waveroll PUBLIC
  JUCE_WEB_BROWSER=0
  JUCE_USE_CURL=0
  JUCE_VST3_CAN_REPLACE_VST2=0
  JUCE_DISPLAY_SPLASH_SCREEN=0)

target_link_libraries(Waveroll PRIVATE
  juce::juce_audio_utils
  PUBLIC
  juce::juce_recommended_config_flags
  juce::juce_recommended_warning_flags)
