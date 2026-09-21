cmake_minimum_required(VERSION 3.22)
project(fluss_isr_storm_cpp LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)

# Point FLUSS_SDK_SOURCE at a fluss-rust checkout that has bindings/cpp.
# In this branch that is the vendored SDK under .repro-cb/fluss-rust, e.g.
#   cmake -S . -B build -DFLUSS_SDK_SOURCE=/abs/path/to/.repro-cb/fluss-rust
if(NOT DEFINED FLUSS_SDK_SOURCE)
  get_filename_component(_default_sdk "${CMAKE_CURRENT_SOURCE_DIR}/../fluss-rust" ABSOLUTE)
  set(FLUSS_SDK_SOURCE "${_default_sdk}" CACHE PATH "fluss-rust source checkout")
endif()
if(NOT EXISTS "${FLUSS_SDK_SOURCE}/bindings/cpp/CMakeLists.txt")
  message(FATAL_ERROR "Set FLUSS_SDK_SOURCE to a fluss-rust source checkout (got: ${FLUSS_SDK_SOURCE})")
endif()

set(FLUSS_ENABLE_TESTING OFF CACHE BOOL "" FORCE)
set(FLUSS_DEV OFF CACHE BOOL "" FORCE)
set(FLUSS_ENABLE_ADDRESS_SANITIZER OFF CACHE BOOL "" FORCE)
add_subdirectory("${FLUSS_SDK_SOURCE}/bindings/cpp" sdk EXCLUDE_FROM_ALL)

find_package(Threads REQUIRED)
add_executable(isr-storm-cpp isr_storm_cpp.cpp)
target_link_libraries(isr-storm-cpp PRIVATE fluss_cpp Threads::Threads)
add_dependencies(isr-storm-cpp cargo_build)
target_compile_options(isr-storm-cpp PRIVATE -Wall -Wextra)
