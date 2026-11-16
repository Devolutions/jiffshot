#import <Cocoa/Cocoa.h>
#import <CoreGraphics/CoreGraphics.h>
#include <errno.h>
#include <signal.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static NSString *binary;
static NSString *cli;
static NSRunningApplication *reference;
static NSRunningApplication *observed;
static BOOL quiet = YES;
static BOOL widgetVisible;
static BOOL noticeVisible;
static NSString *violation;
static NSString *phase = @"launch";

static void sample(void) {
    for (NSRunningApplication *app in NSWorkspace.sharedWorkspace.runningApplications) {
        if (![app.executableURL.path isEqualToString:binary]) continue;
        if (observed && observed.processIdentifier != app.processIdentifier) {
            violation = @"A second Jiffshot instance appeared";
        }
        observed = app;
        if (quiet && app.activationPolicy != NSApplicationActivationPolicyAccessory) {
            violation = [NSString stringWithFormat:@"Background activation policy was %ld",
                         (long)app.activationPolicy];
        }
        if (quiet && app.active) violation = @"Background Jiffshot became active";
    }
    if (quiet && NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier != reference.processIdentifier) {
        NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
        violation = [NSString stringWithFormat:@"Foreground application changed from %@ (%d) to %@ (%d) during %@",
                     reference.localizedName, reference.processIdentifier,
                     front.localizedName, front.processIdentifier, phase];
    }
    if (observed) {
        widgetVisible = NO;
        noticeVisible = NO;
        NSArray *windows = CFBridgingRelease(CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly,
                                                                       kCGNullWindowID));
        if (!windows) violation = @"Could not inspect on-screen windows";
        for (NSDictionary *window in windows) {
            if ([window[(id)kCGWindowOwnerPID] intValue] != observed.processIdentifier ||
                [window[(id)kCGWindowAlpha] doubleValue] <= 0) continue;
            NSDictionary *bounds = window[(id)kCGWindowBounds];
            BOOL widget = [window[(id)kCGWindowName] isEqualToString:@"Jiffshot Capture Widget"] ||
                ([window[(id)kCGWindowLayer] intValue] > 0 &&
                 [bounds[@"Width"] doubleValue] == 90 && [bounds[@"Height"] doubleValue] == 6);
            BOOL notice = [window[(id)kCGWindowName] isEqualToString:@"Jiffshot: Jiffshot hotkeys"] ||
                [window[(id)kCGWindowName] isEqualToString:@"Jiffshot: Jiffshot startup"] ||
                ([window[(id)kCGWindowLayer] intValue] > 0 &&
                 [bounds[@"Width"] doubleValue] >= 200 && [bounds[@"Height"] doubleValue] > 30);
            if (widget) widgetVisible = YES;
            if (notice) noticeVisible = YES;
            if (quiet && ([window[(id)kCGWindowLayer] intValue] == 0 || widget || notice)) {
                violation = widget ? @"Background Jiffshot exposed the Capture Widget"
                           : notice ? @"Background Jiffshot exposed a startup notice"
                                   : @"Background Jiffshot exposed a normal window";
            }
        }
    }
}

static void waitFor(NSTimeInterval seconds) {
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:seconds];
    do {
        [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.005]];
        sample();
    } while (deadline.timeIntervalSinceNow > 0);
}

static NSDictionary *command(NSArray<NSString *> *arguments) {
    phase = [arguments componentsJoinedByString:@" "];
    NSTask *task = [NSTask new];
    task.executableURL = [NSURL fileURLWithPath:cli];
    task.arguments = arguments;
    NSPipe *output = [NSPipe pipe];
    NSPipe *errors = [NSPipe pipe];
    task.standardOutput = output;
    task.standardError = errors;
    NSError *error;
    if (![task launchAndReturnError:&error]) {
        @throw [NSException exceptionWithName:@"LaunchFailure" reason:error.description userInfo:nil];
    }
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:40];
    while (task.running && deadline.timeIntervalSinceNow > 0) waitFor(0.01);
    if (task.running) {
        [task terminate];
        @throw [NSException exceptionWithName:@"Timeout" reason:@"CLI timed out" userInfo:nil];
    }
    [task waitUntilExit];
    NSData *data = [output.fileHandleForReading readDataToEndOfFile];
    NSString *stderrText = [[NSString alloc] initWithData:[errors.fileHandleForReading readDataToEndOfFile]
                                                encoding:NSUTF8StringEncoding];
    id response = [NSJSONSerialization JSONObjectWithData:data options:0 error:&error];
    if (task.terminationStatus != 0 || ![response isKindOfClass:NSDictionary.class] ||
        ![response[@"ok"] boolValue]) {
        NSString *reason = [NSString stringWithFormat:@"%@ failed (%d): %@; %@",
                            arguments, task.terminationStatus,
                            [[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding], stderrText];
        @throw [NSException exceptionWithName:@"CommandFailure" reason:reason userInfo:nil];
    }
    if (violation) @throw [NSException exceptionWithName:@"PresentationFailure" reason:violation userInfo:nil];
    return response[@"result"];
}

static void require(BOOL condition, NSString *message) {
    if (!condition) @throw [NSException exceptionWithName:@"AssertionFailure" reason:message userInfo:nil];
}

static NSRunningApplication *workspaceLaunch(NSDictionary *environment, BOOL newInstance) {
    __block NSRunningApplication *running = nil;
    __block NSError *launchError = nil;
    __block BOOL done = NO;
    NSWorkspaceOpenConfiguration *configuration = NSWorkspaceOpenConfiguration.configuration;
    configuration.environment = environment;
    configuration.createsNewApplicationInstance = newInstance;
    configuration.allowsRunningApplicationSubstitution = NO;
    configuration.addsToRecentItems = NO;
    configuration.promptsUserIfNeeded = NO;
    NSString *bundle = binary.stringByDeletingLastPathComponent.stringByDeletingLastPathComponent.stringByDeletingLastPathComponent;
    [NSWorkspace.sharedWorkspace openApplicationAtURL:[NSURL fileURLWithPath:bundle]
                                         configuration:configuration
                                     completionHandler:^(NSRunningApplication *app, NSError *error) {
        dispatch_async(dispatch_get_main_queue(), ^{
            running = app;
            launchError = error;
            done = YES;
        });
    }];
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:15];
    while (!done && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
    require(done && running != nil, launchError.localizedDescription ?: @"LaunchServices did not start the app");
    return running;
}

static void duplicateBackground(void) {
    phase = @"duplicate --background";
    NSTask *task = [NSTask new];
    task.executableURL = [NSURL fileURLWithPath:binary];
    task.arguments = @[@"--background"];
    NSMutableDictionary *environment = NSProcessInfo.processInfo.environment.mutableCopy;
    [environment removeObjectForKey:@"JIFFSHOT_UITEST"];
    task.environment = environment;
    task.standardOutput = NSFileHandle.fileHandleWithNullDevice;
    task.standardError = NSFileHandle.fileHandleWithNullDevice;
    NSError *error;
    require([task launchAndReturnError:&error], error.description);
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:10];
    while (task.running && deadline.timeIntervalSinceNow > 0) waitFor(0.01);
    if (task.running) {
        [task terminate];
        require(NO, @"Duplicate background launch did not exit");
    }
    [task waitUntilExit];
    require(task.terminationStatus == 0, @"Duplicate background launch failed");
    require(violation == nil, violation);
}

static void stopTask(NSTask *task) {
    if (!task.running) return;
    [task terminate];
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:2];
    while (task.running && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
    if (task.running) kill(task.processIdentifier, SIGKILL);
    [task waitUntilExit];
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc != 4) {
            fprintf(stderr, "usage: background-probe APP_EXECUTABLE CLI TEST_ROOT\n");
            return 2;
        }
        binary = @(argv[1]);
        cli = @(argv[2]);
        NSString *root = @(argv[3]);
        reference = NSWorkspace.sharedWorkspace.frontmostApplication;
        id observer = [NSWorkspace.sharedWorkspace.notificationCenter
            addObserverForName:NSWorkspaceDidActivateApplicationNotification
            object:nil queue:NSOperationQueue.mainQueue usingBlock:^(NSNotification *notification) {
                NSRunningApplication *app = notification.userInfo[NSWorkspaceApplicationKey];
                if (quiet && app.processIdentifier != reference.processIdentifier)
                    violation = [NSString stringWithFormat:@"%@ (%d) activated during %@",
                                 app.localizedName, app.processIdentifier, phase];
            }];
        int result = 0;
        NSTask *foreground = nil;
        NSTask *failed = nil;
        NSString *blockedSocket = nil;
        BOOL blockedSocketCreated = NO;
        @try {
            command(@[@"status"]);
            waitFor(0.5);
            require(observed != nil && !observed.terminated, @"Auto-launch did not create the app");
            pid_t pid = observed.processIdentifier;
            command(@[@"status"]);
            command(@[@"status"]);
            require(observed.processIdentifier == pid, @"Repeated commands did not reuse the app");
            duplicateBackground();
            NSDictionary *document = command(@[@"doc", @"new", @"--width", @"32", @"--height", @"32"]);
            NSString *identifier = [document[@"id"] description];
            require(identifier != nil, @"New document returned no ID");
            command(@[@"doc", @"export", @"--id", identifier, @"--out", [root stringByAppendingPathComponent:@"quiet.png"]]);
            command(@[@"doc", @"close", @"--id", identifier]);
            NSString *video = [root stringByAppendingPathComponent:@"control-video.mp4"];
            NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:10];
            while (![[NSFileManager defaultManager] fileExistsAtPath:[root stringByAppendingPathComponent:@"control-video-ready"]]
                   && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            command(@[@"video", @"info", video]);
            command(@[@"video", @"export", video, @"--out", [root stringByAppendingPathComponent:@"trimmed.mp4"],
                      @"--from", @"0", @"--duration", @"0.2", @"--mute"]);
            waitFor(0.3);
            require(violation == nil, violation);
            require(observed.activationPolicy == NSApplicationActivationPolicyAccessory, @"Quiet commands left Accessory mode");
            printf("PASS: CLI auto-launch, repeated status, duplicate background launch, document export, and video trim stayed accessory/inactive without normal windows\n");

            quiet = NO;
            command(@[@"open"]);
            deadline = [NSDate dateWithTimeIntervalSinceNow:10];
            while ((!observed.active || observed.activationPolicy != NSApplicationActivationPolicyRegular ||
                    !widgetVisible || !noticeVisible)
                   && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            require(observed.processIdentifier == pid && observed.active &&
                    observed.activationPolicy == NSApplicationActivationPolicyRegular,
                    @"Explicit open did not promote the same process to Regular and activate");
            require(widgetVisible, @"Explicit open did not restore the saved Capture Widget");
            require(noticeVisible, @"Explicit open did not show the deferred startup notice");
            printf("PASS: explicit CLI open promoted the existing process and restored the saved widget and startup notice\n");
            command(@[@"quit"]);
            deadline = [NSDate dateWithTimeIntervalSinceNow:15];
            while (!observed.terminated && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            require(observed.terminated, @"Quit left the fixture running");
            printf("PASS: CLI quit shut down the app\n");

            NSMutableDictionary *environment = NSProcessInfo.processInfo.environment.mutableCopy;
            [environment removeObjectForKey:@"JIFFSHOT_UITEST"];
            [environment removeObjectForKey:@"JIFFSHOT_UITEST_CONTROL"];
            [environment removeObjectForKey:@"JIFFSHOT_UITEST_KEEP"];
            [reference activateWithOptions:0];
            waitFor(0.3);
            reference = NSWorkspace.sharedWorkspace.frontmostApplication;
            observed = nil;
            quiet = YES;
            command(@[@"status"]);
            waitFor(0.5);
            require(observed != nil && !observed.terminated, @"Reopen fixture did not start in the background");
            pid = observed.processIdentifier;
            quiet = NO;
            NSRunningApplication *reopened = workspaceLaunch(environment, NO);
            require(reopened.processIdentifier == pid, @"LaunchServices did not reuse the background process");
            deadline = [NSDate dateWithTimeIntervalSinceNow:15];
            while ((!reopened.active || reopened.activationPolicy != NSApplicationActivationPolicyRegular ||
                    NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier != pid) &&
                   !reopened.terminated && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            require(!reopened.terminated && reopened.active &&
                    reopened.activationPolicy == NSApplicationActivationPolicyRegular &&
                    NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier == pid,
                    @"LaunchServices did not promote the background process");
            command(@[@"quit"]);
            deadline = [NSDate dateWithTimeIntervalSinceNow:15];
            while (!reopened.terminated && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            require(reopened.terminated, @"Reopened background process did not quit");
            printf("PASS: LaunchServices reopened and promoted the existing background process\n");

            observed = nil;
            foreground = [NSTask new];
            foreground.executableURL = [NSURL fileURLWithPath:binary];
            foreground.environment = environment;
            foreground.standardOutput = NSFileHandle.fileHandleWithNullDevice;
            foreground.standardError = NSFileHandle.fileHandleWithNullDevice;
            NSError *error;
            require([foreground launchAndReturnError:&error], error.description);
            deadline = [NSDate dateWithTimeIntervalSinceNow:15];
            while (foreground.running &&
                   (!observed || observed.processIdentifier != foreground.processIdentifier ||
                    !observed.active || observed.activationPolicy != NSApplicationActivationPolicyRegular ||
                    NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier != foreground.processIdentifier) &&
                   deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            require(foreground.running && observed.processIdentifier == foreground.processIdentifier &&
                    observed.active && observed.activationPolicy == NSApplicationActivationPolicyRegular &&
                    NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier == foreground.processIdentifier,
                    @"Ordinary launch did not become the foreground Regular application");
            NSDictionary *foregroundStatus = command(@[@"status"]);
            require([foregroundStatus[@"pid"] intValue] == foreground.processIdentifier,
                    @"Ordinary launch did not own the control server");
            blockedSocket = foregroundStatus[@"pipe"];
            require([blockedSocket isKindOfClass:NSString.class] &&
                    [blockedSocket hasSuffix:@"/control.sock"], @"Control socket path is invalid");
            command(@[@"quit"]);
            deadline = [NSDate dateWithTimeIntervalSinceNow:15];
            while (foreground.running && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            require(!foreground.running, @"Ordinary launch did not quit");
            [foreground waitUntilExit];
            require(foreground.terminationStatus == 0, @"Ordinary launch exited with an error");
            printf("PASS: ordinary launch activated as a Regular app and quit cleanly\n");

            observed = nil;
            NSRunningApplication *workspaceApp = workspaceLaunch(environment, YES);
            observed = workspaceApp;
            deadline = [NSDate dateWithTimeIntervalSinceNow:15];
            while ((!workspaceApp.active || workspaceApp.activationPolicy != NSApplicationActivationPolicyRegular ||
                    NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier != workspaceApp.processIdentifier) &&
                   !workspaceApp.terminated && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            require(!workspaceApp.terminated && workspaceApp.active &&
                    workspaceApp.activationPolicy == NSApplicationActivationPolicyRegular &&
                    NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier == workspaceApp.processIdentifier,
                    @"LaunchServices launch did not become the foreground Regular application");
            NSDictionary *workspaceStatus = command(@[@"status"]);
            require([workspaceStatus[@"pid"] intValue] == workspaceApp.processIdentifier,
                    @"LaunchServices launch did not own the control server");
            command(@[@"quit"]);
            deadline = [NSDate dateWithTimeIntervalSinceNow:15];
            while (!workspaceApp.terminated && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
            require(workspaceApp.terminated, @"LaunchServices launch did not quit");
            printf("PASS: LaunchServices launch activated as a Regular app and quit cleanly\n");

            NSString *socketDirectory = blockedSocket.stringByDeletingLastPathComponent;
            for (NSString *name in @[@"control.sock", @"JiffshotCaptureTray.sock"]) {
                blockedSocket = [socketDirectory stringByAppendingPathComponent:name];
                struct stat info;
                if (lstat(blockedSocket.fileSystemRepresentation, &info) == 0) {
                    require(S_ISSOCK(info.st_mode) && unlink(blockedSocket.fileSystemRepresentation) == 0,
                            @"Could not remove the fixture's stale socket");
                } else {
                    require(errno == ENOENT, @"Could not inspect the fixture socket");
                }
                require(mkdir(blockedSocket.fileSystemRepresentation, 0700) == 0,
                        [NSString stringWithFormat:@"Could not block %@: %s", name, strerror(errno)]);
                blockedSocketCreated = YES;
                [reference activateWithOptions:0];
                waitFor(0.3);
                reference = NSWorkspace.sharedWorkspace.frontmostApplication;
                observed = nil;
                quiet = YES;
                phase = [NSString stringWithFormat:@"%@ startup failure", name];
                failed = [NSTask new];
                failed.executableURL = [NSURL fileURLWithPath:binary];
                failed.arguments = @[@"--background"];
                failed.environment = environment;
                failed.standardOutput = NSFileHandle.fileHandleWithNullDevice;
                NSString *errorPath = [root stringByAppendingPathComponent:
                    [NSString stringWithFormat:@"%@-failure-stderr.txt", name]];
                require([[NSFileManager defaultManager] createFileAtPath:errorPath contents:nil attributes:nil],
                        @"Could not create the startup failure log");
                NSFileHandle *errors = [NSFileHandle fileHandleForWritingAtPath:errorPath];
                require(errors != nil, @"Could not open the startup failure log");
                failed.standardError = errors;
                require([failed launchAndReturnError:&error], error.description);
                deadline = [NSDate dateWithTimeIntervalSinceNow:10];
                while (failed.running && deadline.timeIntervalSinceNow > 0) waitFor(0.02);
                require(!failed.running, [NSString stringWithFormat:@"Background app remained alive after %@ failed", name]);
                [failed waitUntilExit];
                [errors closeFile];
                require(failed.terminationStatus != 0, @"Background startup failure reported success");
                NSString *failureLog = [NSString stringWithContentsOfFile:errorPath
                                                                  encoding:NSUTF8StringEncoding error:&error];
                NSString *expectedError = [name isEqualToString:@"control.sock"] ? @"control" : @"capture instance listener";
                require([failureLog containsString:expectedError], @"Background startup failure was not reported");
                require(violation == nil, violation);
                require(rmdir(blockedSocket.fileSystemRepresentation) == 0, @"Could not remove fixture socket blocker");
                blockedSocketCreated = NO;
                printf("PASS: %s startup failure was reported and left no background app running\n", name.UTF8String);
            }
        } @catch (NSException *exception) {
            fprintf(stderr, "FAIL: %s\n", exception.reason.UTF8String);
            result = 1;
        } @finally {
            quiet = NO;
            [NSWorkspace.sharedWorkspace.notificationCenter removeObserver:observer];
            if (observed && !observed.terminated) {
                [observed terminate];
                waitFor(2);
                if (!observed.terminated) {
                    [observed forceTerminate];
                    waitFor(2);
                }
                if (!observed.terminated) {
                    fprintf(stderr, "FAIL: could not stop fixture PID %d\n", observed.processIdentifier);
                    result = 1;
                }
            }
            stopTask(foreground);
            stopTask(failed);
            if (blockedSocketCreated && rmdir(blockedSocket.fileSystemRepresentation) != 0) {
                fprintf(stderr, "FAIL: could not remove the fixture socket blocker: %s\n", strerror(errno));
                result = 1;
            }
            [reference activateWithOptions:0];
        }
        return result;
    }
}
