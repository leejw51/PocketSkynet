const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    // The `pocketskynet` library module: protocol crypto (EIP-191 over
    // secp256k1, Keccak-256, msgHash), the two HTTP transports and the typed
    // API client.
    const mod = b.addModule("pocketskynet", .{
        .root_source_file = b.path("src/root.zig"),
        .target = target,
        .optimize = optimize,
        // `std.c.waitpid` / `std.c.getenv` are used for process supervision
        // and environment lookups.
        .link_libc = true,
    });

    const exe = b.addExecutable(.{
        .name = "pskynet-zig",
        .root_module = b.createModule(.{
            .root_source_file = b.path("src/main.zig"),
            .target = target,
            .optimize = optimize,
            .link_libc = true,
            .imports = &.{
                .{ .name = "pocketskynet", .module = mod },
            },
        }),
    });
    b.installArtifact(exe);

    // ---- unit tests (no server required) -----------------------------------
    const unit_tests = b.addTest(.{ .root_module = mod });
    const run_unit = b.addRunArtifact(unit_tests);

    const vector_tests = b.addTest(.{
        .root_module = b.createModule(.{
            .root_source_file = b.path("tests/vectors.zig"),
            .target = target,
            .optimize = optimize,
            .link_libc = true,
            .imports = &.{
                .{ .name = "pocketskynet", .module = mod },
            },
        }),
    });
    const run_vectors = b.addRunArtifact(vector_tests);

    const test_step = b.step("test", "Run unit tests (no server needed)");
    test_step.dependOn(&run_unit.step);
    test_step.dependOn(&run_vectors.step);

    // ---- integration tests (spawn a real pocketskynet server) --------------
    const itest_options = b.addOptions();
    itest_options.addOptionPath("cli_exe", exe.getEmittedBin());

    const integration_tests = b.addTest(.{
        .root_module = b.createModule(.{
            .root_source_file = b.path("tests/integration.zig"),
            .target = target,
            .optimize = optimize,
            .link_libc = true,
            .imports = &.{
                .{ .name = "pocketskynet", .module = mod },
                .{ .name = "itest_options", .module = itest_options.createModule() },
            },
        }),
    });
    const run_itest = b.addRunArtifact(integration_tests);

    const itest_step = b.step("itest", "Run integration tests against a real pocketskynet server");
    itest_step.dependOn(&run_itest.step);

    const test_all_step = b.step("test-all", "Run unit and integration tests");
    test_all_step.dependOn(test_step);
    test_all_step.dependOn(itest_step);
}
