cmake_minimum_required(VERSION 3.21)
project(tp-vcam LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
# Static CRT: FrameServer loads the DLL into a service process where a
# missing or mismatched VC++ redistributable fails silently.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")

add_compile_definitions(UNICODE _UNICODE NOMINMAX)
add_compile_options(/W4 /permissive- /utf-8 /wd4324)

add_library(tp-vcam SHARED
    src/dllmain.cpp
    src/activator.cpp
    src/media_source.cpp
    src/media_stream.cpp
    src/sink_reader.cpp
    src/render.cpp
    src/tp-vcam.def
)
target_include_directories(tp-vcam PRIVATE src ${CMAKE_CURRENT_SOURCE_DIR}/../../common)
# runtimeobject: WRL::Module pulls in RoOriginateError even for classic COM.
target_link_libraries(tp-vcam PRIVATE mfplat mfuuid advapi32 ole32 runtimeobject)

add_executable(tp-vcam-register tools/register.cpp)
target_include_directories(tp-vcam-register PRIVATE src)
target_link_libraries(tp-vcam-register PRIVATE mfsensorgroup mfplat ole32)

# FrameServer runs as LocalService, which cannot read files under a user
# profile, so the DLL has to live somewhere world-readable before registering.
if(CMAKE_INSTALL_PREFIX_INITIALIZED_TO_DEFAULT)
    set(CMAKE_INSTALL_PREFIX "C:/Program Files/TelePresence/VirtualCamera" CACHE PATH "" FORCE)
endif()
install(TARGETS tp-vcam tp-vcam-register RUNTIME DESTINATION .)
