# Copyright (c) Facebook, Inc. and its affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Adding header files in CMake will add them to Visual Studio project
file(GLOB CONFIG_SRCS "*.cpp" "*.h")
add_library(
  eden_win_utils STATIC
    ${CONFIG_SRCS}
)

target_link_libraries(
  eden_win_utils
  PUBLIC
    eden_model
    eden_utils
    Userenv.lib
    Folly::folly
)

add_subdirectory(test)
