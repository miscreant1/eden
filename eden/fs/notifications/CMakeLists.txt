# Copyright (c) Facebook, Inc. and its affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

file(GLOB NOTIFICATIONS_SRCS "*.cpp")
add_library(
  eden_notifications STATIC
  ${NOTIFICATIONS_SRCS}
)
target_link_libraries(
  eden_notifications
  PUBLIC
    eden_config
)
