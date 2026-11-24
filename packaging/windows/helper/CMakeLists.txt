cmake_minimum_required(VERSION 3.21)
project(tp-setup-helper LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
# Static CRT: as a pre_install hook this runs before anything else in the
# payload is on disk, so it may depend on nothing but the OS.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
add_compile_definitions(UNICODE _UNICODE NOMINMAX)
add_compile_options(/W4 /permissive- /utf-8)

add_executable(tp-setup-helper tp-setup-helper.cpp)
target_link_libraries(tp-setup-helper PRIVATE advapi32 user32)
