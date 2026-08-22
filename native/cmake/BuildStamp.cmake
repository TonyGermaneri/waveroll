# Writes a one-line build stamp the editor can display.
#
# Live's plugin fingerprint is <size>:<mtime> of the binary, so a rebuild is always detected and
# Rescan does pick it up. What is missing without this is any way to *see* which build is loaded --
# and "is this the one I just built" is the question that gets asked twenty times a day.
#
# Run at build time rather than configure time, and written through copy_if_different so an
# unchanged stamp does not relink the plugin for nothing.
find_package(Git QUIET)
set(sha "unknown")
set(count "0")
set(dirty "")
if(GIT_FOUND)
  execute_process(COMMAND ${GIT_EXECUTABLE} rev-parse --short HEAD
    WORKING_DIRECTORY ${SOURCE_DIR} OUTPUT_VARIABLE sha
    OUTPUT_STRIP_TRAILING_WHITESPACE ERROR_QUIET)
  execute_process(COMMAND ${GIT_EXECUTABLE} rev-list --count HEAD
    WORKING_DIRECTORY ${SOURCE_DIR} OUTPUT_VARIABLE count
    OUTPUT_STRIP_TRAILING_WHITESPACE ERROR_QUIET)
  execute_process(COMMAND ${GIT_EXECUTABLE} status --porcelain
    WORKING_DIRECTORY ${SOURCE_DIR} OUTPUT_VARIABLE changes
    OUTPUT_STRIP_TRAILING_WHITESPACE ERROR_QUIET)
  if(NOT changes STREQUAL "")
    set(dirty "+")
  endif()
endif()
string(TIMESTAMP now "%Y-%m-%d %H:%M" UTC)

file(WRITE "${OUT}.tmp"
"// Generated. Do not edit.
namespace waveroll { const char* buildStamp() { return \"0.1.${count}  ${sha}${dirty}  built ${now}Z\"; } }
")
execute_process(COMMAND ${CMAKE_COMMAND} -E copy_if_different "${OUT}.tmp" "${OUT}")
file(REMOVE "${OUT}.tmp")
