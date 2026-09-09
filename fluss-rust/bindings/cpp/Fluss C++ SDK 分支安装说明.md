<!-- SPDX-License-Identifier: Apache-2.0 -->

# Fluss C++ SDK 分支安装说明

本说明适用于已按[原安装文档](https://clients.fluss.apache.org/user-guide/cpp/installation/)完成环境配置的用户。沿用现有编译环境和项目接入方式，只更换 SDK 源码版本，无需重新安装依赖。

## 源码版本

- 仓库：`https://github.com/naivedogger/fluss.git`
- 云览客户分支：`bugfix/yunlan-cpp-sdk`
- 本文对应的 SDK 代码提交：`c19493ff208b5e22dd222b2e7e0672b5e42a6c46`
- 相关上游 PR：[4252](https://github.com/apache/fluss/pull/4252)

该提交包含旧乱序响应的重试判断修复，以及重试批次排在新批次之前的队列排序修复。

该分支用于汇集云览客户需要的修复，位于开发者 fork，不是 Apache 官方发布版本。本说明用于隔离测试环境中的定向验证。后续纳入其他修复时，会同步更新本文中的 SDK 提交号；请使用明确指定的提交，不要自动跟随分支最新代码。

## 从源码编译

在新目录拉取源码，保留原有 SDK，便于回退：

```bash
git clone --single-branch \
  --branch bugfix/yunlan-cpp-sdk \
  https://github.com/naivedogger/fluss.git fluss-sdk-write-fix

cd fluss-sdk-write-fix
git checkout --detach c19493ff208b5e22dd222b2e7e0672b5e42a6c46
```

在仓库根目录执行：

```bash
cmake -S fluss-rust/bindings/cpp -B build/cpp \
  -DCMAKE_BUILD_TYPE=Release
cmake --build build/cpp --parallel 2
```

如原来通过 `CMAKE_PREFIX_PATH` 或 `FLUSS_CPP_ARROW_SYSTEM_ROOT` 指定 Arrow 安装位置，在配置命令中保留相同参数。该分支需要 CMake 3.22 或以上版本，继续使用已配好的 C++17 编译器、Rust 工具链和 Arrow。

与原文档中的独立 `fluss-rust` 仓库不同，这里的 C++ 源码子目录是 `fluss-rust/bindings/cpp`。

主要文件位置如下，均相对于本次获取的仓库根目录：

| 文件 | 位置 |
| --- | --- |
| C++ 静态库 | `build/cpp/libfluss_cpp.a` |
| Rust FFI 静态库 | `fluss-rust/target/release/libfluss_cpp.a` |
| 公共头文件 | `fluss-rust/bindings/cpp/include/` |
| C++ 示例程序 | `build/cpp/fluss_cpp_example` |

两个静态库虽然同名，但不是同一份文件。如果原来采用手工链接方式，需要同时更新两个库及配套头文件、生成的桥接文件，并沿用已有的 Arrow 和系统库链接配置；不要只替换 C++ 静态库，也不要把两个同名库复制到同一个目录互相覆盖。下面的 CMake 接入方式会管理这些依赖。

## 集成到项目

以下方式选择一种即可，不要在同一项目中重复引入 SDK。

### 方式一：CMake FetchContent

如果项目原来使用 `FetchContent`，将原声明替换为：

```cmake
include(FetchContent)

FetchContent_Declare(
    fluss-cpp
    GIT_REPOSITORY https://github.com/naivedogger/fluss.git
    GIT_TAG c19493ff208b5e22dd222b2e7e0672b5e42a6c46
    SOURCE_SUBDIR fluss-rust/bindings/cpp
)
FetchContent_MakeAvailable(fluss-cpp)

# your_target 替换为项目中已经定义的目标名称。
target_link_libraries(your_target PRIVATE fluss_cpp)
```

使用一个新的构建目录重新配置和编译项目，保留原来的 CMake 参数。采用此方式时，不需要提前手工克隆或单独编译 SDK。

`GIT_TAG` 固定为本文对应的 SDK 提交，避免客户分支后续更新改变构建版本。不要同时设置 `GIT_SHALLOW TRUE`。若该提交无法获取，请联系提供方确认版本，不要直接切换到 main。

### 方式二：本地源码子目录

如果项目原来使用 `add_subdirectory`，完成前面的源码获取步骤后，将目录改为新源码的位置：

```cmake
add_subdirectory(
    /path/to/fluss-sdk-write-fix/fluss-rust/bindings/cpp
    ${CMAKE_CURRENT_BINARY_DIR}/fluss-cpp
)
target_link_libraries(your_target PRIVATE fluss_cpp)
```

将 `/path/to/fluss-sdk-write-fix` 替换为实际的源码目录，`your_target` 替换为已有目标名称，再按原来的方式构建项目。

## 版本确认与回退

手工获取源码时，在新仓库目录执行 `git rev-parse HEAD`，结果应为本文列出的完整提交号。使用 FetchContent 时，核对其下载源码目录中的提交号。

应用重新链接后再替换旧程序；仅下载源码不会更新已有可执行文件。如需回退，将项目的 SDK 路径或 FetchContent 配置恢复为原版本，使用独立构建目录重新编译。

如遇到问题，请提供 SDK 提交号、完整构建报错，或运行时的错误码及完整 `error_message`。

## 参考

- [原 C++ 安装文档](https://clients.fluss.apache.org/user-guide/cpp/installation/)
- [本文对应版本的 CMake 配置](https://github.com/naivedogger/fluss/blob/c19493ff208b5e22dd222b2e7e0672b5e42a6c46/fluss-rust/bindings/cpp/CMakeLists.txt)
- [CMake FetchContent 文档](https://cmake.org/cmake/help/latest/module/FetchContent.html)
- [CMake Git 下载选项](https://cmake.org/cmake/help/latest/module/ExternalProject.html#git)
