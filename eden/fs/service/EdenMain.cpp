/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/service/EdenMain.h"

#include <fb303/FollyLoggingHandler.h>
#include <fb303/TFunctionStatHandler.h>
#include <folly/Conv.h>
#include <folly/ScopeGuard.h>
#include <folly/experimental/FunctionScheduler.h>
#include <folly/init/Init.h>
#include <folly/logging/Init.h>
#include <folly/logging/xlog.h>
#include <folly/ssl/Init.h>
#include <folly/stop_watch.h>
#include <gflags/gflags.h>
#include <pwd.h>
#include <sysexits.h>
#include <thrift/lib/cpp2/server/ThriftServer.h>
#include <unistd.h>
#include <optional>
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/eden-config.h"
#include "eden/fs/fuse/privhelper/PrivHelper.h"
#include "eden/fs/fuse/privhelper/PrivHelperImpl.h"
#include "eden/fs/service/EdenInit.h"
#include "eden/fs/service/EdenServer.h"
#include "eden/fs/service/EdenServiceHandler.h" // for kServiceName
#include "eden/fs/service/StartupLogger.h"
#include "eden/fs/service/Systemd.h"
#include "eden/fs/telemetry/SessionInfo.h"
#include "eden/fs/telemetry/StructuredLogger.h"
#include "eden/fs/utils/UserInfo.h"

// This has to be placed after eden-config.h
#ifdef EDEN_HAVE_CURL
#include <curl/curl.h> // @manual
#endif

DEFINE_bool(edenfs, false, "This legacy argument is ignored.");
DEFINE_bool(allowRoot, false, "Allow running eden directly as root");
DEFINE_bool(
    noWaitForMounts,
    false,
    "Report successful startup without waiting for all configured mounts "
    "to be remounted.");

// Set the default log level for all eden logs to DBG2
// Also change the "default" log handler (which logs to stderr) to log
// messages asynchronously rather than blocking in the logging thread.
FOLLY_INIT_LOGGING_CONFIG("eden=DBG2; default:async=true");

using folly::StringPiece;
using std::string;

namespace {
using namespace facebook::eden;

SessionInfo makeSessionInfo(
    const UserInfo& userInfo,
    std::string hostname,
    std::string edenVersion) {
  SessionInfo env;
  env.username = userInfo.getUsername();
  env.hostname = std::move(hostname);
  env.os = getOperatingSystemName();
  env.osVersion = getOperatingSystemVersion();
  env.edenVersion = std::move(edenVersion);
  return env;
}
} // namespace

namespace facebook {
namespace eden {

std::string DefaultEdenMain::getEdenfsBuildName() {
  StringPiece version(EDEN_VERSION);
  StringPiece release(EDEN_RELEASE);

  if (!version.empty()) {
    return folly::to<string>("edenfs ", version, "-", release);
  }

  // Assume this is a development build if EDEN_VERSION is unset.
  return "edenfs (dev build)";
}

std::string DefaultEdenMain::getEdenfsVersion() {
  StringPiece version(EDEN_VERSION);

  if (!version.empty()) {
    return folly::to<string>(version);
  }

  return "(dev build)";
}

std::string DefaultEdenMain::getLocalHostname() {
  return getHostname();
}

void DefaultEdenMain::prepare(const EdenServer& /*server*/) {
  fb303::registerFollyLoggingOptionHandlers();
}

void DefaultEdenMain::runServer(const EdenServer& server) {
  // ThriftServer::serve() will drive the current thread's EventBase.
  // Verify that we are being called from the expected thread, and will end up
  // driving the EventBase returned by EdenServer::getMainEventBase().
  CHECK_EQ(
      server.getMainEventBase(),
      folly::EventBaseManager::get()->getEventBase());

  fb303::fbData->setExportedValue("build_package_name", EDEN_PACKAGE_NAME);
  fb303::fbData->setExportedValue("build_package_version", EDEN_VERSION);
  fb303::fbData->setExportedValue("build_package_release", EDEN_RELEASE);
  fb303::fbData->setExportedValue("build_revision", EDEN_BUILD_REVISION);
  fb303::fbData->setExportedValue(
      "build_time_unix", folly::to<std::string>(EDEN_BUILD_TIME_UNIX));

  fb303::withThriftFunctionStats(kServiceName, server.getHandler().get(), [&] {
    server.getServer()->serve();
  });
}

int runEdenMain(EdenMain&& main, int argc, char** argv) {
  ////////////////////////////////////////////////////////////////////
  // Running as root: do not add any new code here.
  // EdenFS normally starts with root privileges so it can perform mount
  // operations.  We should be very careful about anything we do here
  // before we have dropped privileges.  In general do not add any new
  // code here at the start of main: new initialization logic should
  // only go after the "Root privileges dropped" comment below.
  ////////////////////////////////////////////////////////////////////

  // Fork the privhelper process, then drop privileges in the main process.
  // This should be done as early as possible, so that everything else we do
  // runs only with normal user privileges.
  //
  // We do this even before calling folly::init().  The privhelper server
  // process will call folly::init() on its own.
  auto identity = UserInfo::lookup();
  auto originalEUID = geteuid();
  auto privHelper = startPrivHelper(identity);
  identity.dropPrivileges();

  ////////////////////////////////////////////////////////////////////
  //// Root privileges dropped
  ////////////////////////////////////////////////////////////////////

  folly::stop_watch<> daemonStart;

  std::vector<std::string> originalCommandLine{argv, argv + argc};

  // This is normally performed just-in-time by folly::ssl::SSLContext,
  // but we need to explicitly ensure that it is initialized
  // prior to initializing libcurl
  folly::ssl::init();

#ifdef EDEN_HAVE_CURL
  // We need to call curl_global_init before any thread is created to avoid
  // crashes happens when curl structs are passed between threads.
  // See curl's documentation for details.
  curl_global_init(CURL_GLOBAL_ALL);
  SCOPE_EXIT {
    curl_global_cleanup();
  };
#endif

  // Make sure to run this before any flag values are read.
  folly::init(&argc, &argv);
  if (argc != 1) {
    fprintf(stderr, "error: unexpected trailing command line arguments\n");
    return EX_USAGE;
  }

  // Fail if we were not started as root.  The privhelper needs root
  // privileges in order to perform mount and unmount operations.
  // We check this after calling folly::init() so that non-root users
  // can use the --help argument.
  if (originalEUID != 0) {
    fprintf(stderr, "error: edenfs must be started as root\n");
    return EX_NOPERM;
  }

  if (identity.getUid() == 0 && !FLAGS_allowRoot) {
    fprintf(
        stderr,
        "error: you appear to be running eden as root, "
        "rather than using\n"
        "sudo or a setuid binary.  This is normally undesirable.\n"
        "Pass in the --allowRoot flag if you really mean to run "
        "eden as root.\n");
    return EX_USAGE;
  }

#if EDEN_HAVE_SYSTEMD
  if (FLAGS_experimentalSystemd) {
    XLOG(INFO) << "Running in experimental systemd mode";
  }
#endif

  std::unique_ptr<EdenConfig> edenConfig;
  try {
    edenConfig = getEdenConfig(identity);
  } catch (const ArgumentError& ex) {
    fprintf(stderr, "%s\n", ex.what());
    return EX_SOFTWARE;
  }

  auto logPath = getLogPath(edenConfig->edenDir.getValue());
  auto startupLogger = daemonizeIfRequested(logPath);
  XLOG(DBG3) << edenConfig->toString();
  std::optional<EdenServer> server;
  auto prepareFuture = folly::Future<folly::Unit>::makeEmpty();
  try {
    // If stderr was redirected to a log file, inform the privhelper
    // to make sure it logs to our current stderr.
    if (!logPath.empty()) {
      privHelper->setLogFileBlocking(
          folly::File(STDERR_FILENO, /*ownsFd=*/false));
    }

    privHelper->setDaemonTimeoutBlocking(
        edenConfig->fuseDaemonTimeout.getValue());

    // Since we are a daemon, and we don't ever want to be in a situation
    // where we hold any open descriptors through a fuse mount that points
    // to ourselves (which can happen during takeover), we chdir to `/`
    // to avoid having our cwd reference ourselves if the user runs
    // `eden daemon --takeover` from within an eden mount
    folly::checkPosixError(chdir("/"), "failed to chdir(/)");

    // Set some default glog settings, to be applied unless overridden on the
    // command line
    gflags::SetCommandLineOptionWithMode(
        "logtostderr", "1", gflags::SET_FLAGS_DEFAULT);
    gflags::SetCommandLineOptionWithMode(
        "minloglevel", "1", gflags::SET_FLAGS_DEFAULT);

    startupLogger->log(
        "Starting ", main.getEdenfsBuildName(), ", pid ", getpid());

    auto sessionInfo = makeSessionInfo(
        identity, main.getLocalHostname(), main.getEdenfsVersion());
    server.emplace(
        std::move(originalCommandLine),
        std::move(identity),
        std::move(sessionInfo),
        std::move(privHelper),
        std::move(edenConfig),
        main.getEdenfsVersion());

    prepareFuture = server->prepare(startupLogger, !FLAGS_noWaitForMounts);
  } catch (const std::exception& ex) {
    auto startTimeInSeconds =
        std::chrono::duration<double>{daemonStart.elapsed()}.count();
    server->getServerState()->getStructuredLogger()->logEvent(
        DaemonStart{startTimeInSeconds, FLAGS_takeover, false /*success*/});
    startupLogger->exitUnsuccessfully(
        EX_SOFTWARE, "error starting edenfs: ", folly::exceptionStr(ex));
  }

  std::move(prepareFuture)
      .thenTry([startupLogger](folly::Try<folly::Unit>&& result) {
        // If an error occurred this means that we failed to mount all of the
        // mount points.  However, we have still started and will continue
        // running, so we report successful startup here no matter what.
        if (result.hasException()) {
          // Log an overall error message here.
          // We will have already logged more detailed messages for each mount
          // failure when it occurred.
          startupLogger->warn(
              "did not successfully remount all repositories: ",
              result.exception().what());
        }
        startupLogger->success();
      })
      .ensure(
          [daemonStart,
           structuredLogger = server->getServerState()->getStructuredLogger(),
           takeover = FLAGS_takeover] {
            auto startTimeInSeconds =
                std::chrono::duration<double>{daemonStart.elapsed()}.count();
            // Here we log a success even if we did not successfully remount
            // all repositories (if prepareFuture had an exception). In the
            // future it would be helpful to log number of successful vs
            // unsuccessful remounts
            structuredLogger->logEvent(
                DaemonStart{startTimeInSeconds, takeover, true /*success*/});
          });

  main.prepare(server.value());
  while (true) {
    main.runServer(server.value());
    if (server->performCleanup()) {
      break;
    }
    // performCleanup() returns false if a takeover shutdown attempt
    // failed.  Continue and re-run the server in this case.
  }

  XLOG(INFO) << "edenfs exiting successfully";
  return EX_OK;
}

} // namespace eden
} // namespace facebook
