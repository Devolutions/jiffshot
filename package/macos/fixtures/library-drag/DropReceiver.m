#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ImageIO/ImageIO.h>
#include <limits.h>
#include <math.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static NSArray<NSString *> *expectedPaths;
static NSArray<NSNumber *> *expectedWidths;
static NSArray<NSNumber *> *expectedHeights;
static NSWindow *fixtureWindow;
static CGPoint originalPointer;
static CGPoint targetPointer;
static BOOL movedPointer;
static BOOL finished;
static BOOL releasedButton;
static int result = 1;
static pid_t sourcePID;
static dispatch_source_t sourceMonitor;

static BOOL postMouse(CGEventType type);
static BOOL otherInputActive(void);

static BOOL sourceIsAlive(void) {
    return sourcePID > 1 && getppid() == sourcePID;
}

static BOOL recordReceiverExit(void) {
    NSString *path = NSProcessInfo.processInfo.environment[@"JIFFSHOT_UITEST_DRAG_RECEIVER_EXITED_FILE"];
    NSError *error = nil;
    NSData *data = [@"exited" dataUsingEncoding:NSUTF8StringEncoding];
    if (!path || ![data writeToFile:path options:NSDataWritingAtomic error:&error]) {
        fprintf(stderr, "could not record the receiver's exit: %s\n", error.description.UTF8String ?: "missing path");
        return NO;
    }
    return YES;
}

static BOOL writePointerState(BOOL moved) {
    NSString *pointerFile = NSProcessInfo.processInfo.environment[@"JIFFSHOT_UITEST_DRAG_POINTER_FILE"];
    NSError *error = nil;
    NSData *data = [NSJSONSerialization dataWithJSONObject:@[
        @(originalPointer.x), @(originalPointer.y), @(targetPointer.x), @(targetPointer.y), @(moved)
    ] options:0 error:&error];
    if (!pointerFile || !data || ![data writeToFile:pointerFile options:NSDataWritingAtomic error:&error]) {
        fprintf(stderr, "could not publish the fixture pointer state: %s\n", error.description.UTF8String ?: "missing path");
        return NO;
    }
    return YES;
}

static BOOL pointerPosition(CGPoint *point) {
    CGEventRef event = CGEventCreate(NULL);
    if (!event) return NO;
    *point = CGEventGetLocation(event);
    CFRelease(event);
    return YES;
}

static BOOL samePoint(CGPoint a, CGPoint b) {
    return fabs(a.x - b.x) < 1.0 && fabs(a.y - b.y) < 1.0;
}

static NSInteger targetOwner(void) {
    CGFloat mainHeight = CGDisplayBounds(CGMainDisplayID()).size.height;
    NSPoint point = NSMakePoint(targetPointer.x, mainHeight - targetPointer.y);
    return [NSWindow windowNumberAtPoint:point belowWindowWithWindowNumber:0];
}

static BOOL fixtureOwnsTarget(void) {
    return fixtureWindow && fixtureWindow.isVisible && fixtureWindow.windowNumber > 0
        && targetOwner() == fixtureWindow.windowNumber;
}

static void finish(int status, const char *message) {
    if (finished) return;
    finished = YES;
    if (movedPointer) {
        CGPoint current;
        if (!pointerPosition(&current)) {
            fprintf(stderr, "could not inspect the pointer before restoring it\n");
            status = 1;
        } else if (samePoint(current, targetPointer) && !otherInputActive()) {
            if (!releasedButton && fixtureOwnsTarget() &&
                    CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) {
                releasedButton = postMouse(kCGEventLeftMouseUp);
                if (!releasedButton) status = 1;
            }
            for (int attempt = 0; attempt < 50; attempt++) {
                if (!CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) break;
                usleep(5000);
            }
            CGPoint latest;
            if (!pointerPosition(&latest) || !samePoint(latest, targetPointer) || otherInputActive()
                    || CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) {
                fprintf(stderr, "input changed during drop cleanup; leaving the pointer at its newer position\n");
                status = 1;
            } else {
                CGError error = CGWarpMouseCursorPosition(originalPointer);
                if (error != kCGErrorSuccess) {
                    fprintf(stderr, "could not restore the pointer: %d\n", error);
                    status = 1;
                }
            }
        } else {
            fprintf(stderr, "input changed during the drop; leaving the pointer at its newer position\n");
            status = 1;
        }
        movedPointer = NO;
    }
    if (message) fprintf(status == 0 ? stdout : stderr, "%s\n", message);
    result = status;
    [NSApp stop:nil];
    NSEvent *event = [NSEvent otherEventWithType:NSEventTypeApplicationDefined
        location:NSZeroPoint modifierFlags:0 timestamp:0 windowNumber:0
        context:nil subtype:0 data1:0 data2:0];
    [NSApp postEvent:event atStart:NO];
}

static BOOL validatePasteboard(NSPasteboard *pasteboard) {
    NSInteger changeCount = pasteboard.changeCount;
    NSArray<NSURL *> *urls = [pasteboard readObjectsForClasses:@[[NSURL class]]
        options:@{NSPasteboardURLReadingFileURLsOnlyKey: @YES}];
    if (urls.count != expectedPaths.count || pasteboard.changeCount != changeCount) return NO;
    NSMutableSet<NSString *> *remaining = [NSMutableSet setWithArray:expectedPaths];
    for (NSURL *url in urls) {
        NSUInteger index = [expectedPaths indexOfObject:url.path];
        if (!url.isFileURL || index == NSNotFound || ![remaining containsObject:url.path]) return NO;
        [remaining removeObject:url.path];
        CGImageSourceRef image = CGImageSourceCreateWithURL((__bridge CFURLRef)url, NULL);
        if (!image) return NO;
        NSDictionary *properties = CFBridgingRelease(CGImageSourceCopyPropertiesAtIndex(image, 0, NULL));
        CFRelease(image);
        if ([properties[(__bridge NSString *)kCGImagePropertyPixelWidth] intValue] != expectedWidths[index].intValue
                || [properties[(__bridge NSString *)kCGImagePropertyPixelHeight] intValue] != expectedHeights[index].intValue) {
            return NO;
        }
    }
    return remaining.count == 0 && pasteboard.changeCount == changeCount;
}

@interface DropView : NSView
@property(nonatomic) BOOL received;
@end

@implementation DropView
- (NSDragOperation)draggingEntered:(id<NSDraggingInfo>)sender {
    return (sender.draggingSourceOperationMask & NSDragOperationCopy) ? NSDragOperationCopy : NSDragOperationNone;
}
- (NSDragOperation)draggingUpdated:(id<NSDraggingInfo>)sender {
    return [self draggingEntered:sender];
}
- (BOOL)prepareForDragOperation:(id<NSDraggingInfo>)sender {
    return (sender.draggingSourceOperationMask & NSDragOperationCopy) != 0;
}
- (BOOL)performDragOperation:(id<NSDraggingInfo>)sender {
    self.received = validatePasteboard(sender.draggingPasteboard);
    return self.received;
}
- (void)concludeDragOperation:(id<NSDraggingInfo>)sender {
    (void)sender;
    if (self.received) {
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), dispatch_get_main_queue(), ^{
            finish(0, "external AppKit receiver accepted both dragged images");
        });
    }
}
@end

static BOOL postMouse(CGEventType type) {
    CGPoint current;
    if (!fixtureOwnsTarget() || !pointerPosition(&current) || !samePoint(current, targetPointer) || otherInputActive()) {
        fprintf(stderr, "the drop fixture is covered or input has changed\n");
        return NO;
    }
    CGEventRef event = CGEventCreateMouseEvent(NULL, type, targetPointer, kCGMouseButtonLeft);
    if (!event) {
        fprintf(stderr, "could not create the fixture mouse event\n");
        return NO;
    }
    CGEventSetIntegerValueField(event, kCGMouseEventClickState, 1);
    CGEventPost(kCGHIDEventTap, event);
    CFRelease(event);
    return YES;
}

static BOOL otherMouseButtonDown(void) {
    for (CGMouseButton button = 1; button < 32; button++) {
        if (CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, button)) return YES;
    }
    return NO;
}

static BOOL otherInputActive(void) {
    CGEventFlags modifiers = kCGEventFlagMaskShift | kCGEventFlagMaskControl
        | kCGEventFlagMaskAlternate | kCGEventFlagMaskCommand;
    return otherMouseButtonDown()
        || (CGEventSourceFlagsState(kCGEventSourceStateCombinedSessionState) & modifiers);
}

static int recoverSyntheticButton(const char *xArgument, const char *yArgument) {
    char *end = NULL;
    double x = strtod(xArgument, &end);
    if (end == xArgument || *end != '\0' || !isfinite(x)) return 2;
    double y = strtod(yArgument, &end);
    if (end == yArgument || *end != '\0' || !isfinite(y)) return 2;
    targetPointer = CGPointMake(x, y);
    if (!CGPreflightPostEventAccess()) {
        fprintf(stderr, "posting recovery input is unavailable\n");
        return 1;
    }
    if (!CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) {
        puts("the fixture mouse button was already released");
        return 0;
    }
    if (otherInputActive()) {
        fprintf(stderr, "other input is active; refusing recovery input\n");
        return 1;
    }
    CGPoint current;
    if (!pointerPosition(&current) || !samePoint(current, targetPointer)) {
        fprintf(stderr, "the pointer moved before recovery; refusing recovery input\n");
        return 1;
    }

    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    [NSApp finishLaunching];
    CGFloat mainHeight = CGDisplayBounds(CGMainDisplayID()).size.height;
    NSRect frame = NSMakeRect(x - 15, mainHeight - y - 15, 30, 30);
    fixtureWindow = [[NSWindow alloc] initWithContentRect:frame styleMask:NSWindowStyleMaskBorderless
        backing:NSBackingStoreBuffered defer:NO];
    if (!fixtureWindow) {
        fprintf(stderr, "could not create the recovery window\n");
        return 1;
    }
    fixtureWindow.releasedWhenClosed = NO;
    fixtureWindow.level = NSStatusWindowLevel + 2;
    fixtureWindow.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces
        | NSWindowCollectionBehaviorFullScreenAuxiliary | NSWindowCollectionBehaviorIgnoresCycle;
    fixtureWindow.backgroundColor = NSColor.redColor;
    [fixtureWindow setFrame:frame display:YES];
    [fixtureWindow orderFrontRegardless];
    [fixtureWindow display];
    [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.1]];

    int status = 1;
    if (!fixtureOwnsTarget() || !pointerPosition(&current) || !samePoint(current, targetPointer)) {
        fprintf(stderr, "the recovery window does not own the unchanged pointer position "
            "(owner %ld, fixture %ld, visible %d, pointer %.1f,%.1f, target %.1f,%.1f)\n",
            (long)targetOwner(), (long)fixtureWindow.windowNumber, fixtureWindow.isVisible,
            current.x, current.y, targetPointer.x, targetPointer.y);
    } else if (otherInputActive()) {
        fprintf(stderr, "other input became active; refusing recovery input\n");
    } else if (!CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) {
        puts("the fixture mouse button was already released");
        status = 0;
    } else if (postMouse(kCGEventLeftMouseUp)) {
        for (int attempt = 0; attempt < 50; attempt++) {
            if (!CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) break;
            usleep(5000);
        }
        if (CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) {
            fprintf(stderr, "the fixture mouse button remained pressed after recovery\n");
        } else {
            puts("released the fixture mouse button");
            status = 0;
        }
    }
    [fixtureWindow close];
    return status;
}

static int runDropReceiver(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc != 7) {
            fprintf(stderr, "expected two paths and their image dimensions\n");
            return 2;
        }
        NSMutableArray<NSString *> *paths = [NSMutableArray array];
        NSMutableArray<NSNumber *> *widths = [NSMutableArray array];
        NSMutableArray<NSNumber *> *heights = [NSMutableArray array];
        for (int index = 1; index < argc; index += 3) {
            NSString *path = [NSString stringWithUTF8String:argv[index]];
            int width = atoi(argv[index + 1]);
            int height = atoi(argv[index + 2]);
            if (!path || !path.isAbsolutePath || [paths containsObject:path] || width <= 0 || height <= 0) {
                fprintf(stderr, "invalid expected fixture images\n");
                return 2;
            }
            [paths addObject:path];
            [widths addObject:@(width)];
            [heights addObject:@(height)];
        }
        expectedPaths = paths;
        expectedWidths = widths;
        expectedHeights = heights;
        if (!validatePasteboard([NSPasteboard pasteboardWithName:NSPasteboardNameDrag])) {
            fprintf(stderr, "the active drag pasteboard did not contain the fixture images\n");
            return 1;
        }
        if (!CGPreflightPostEventAccess()) {
            fprintf(stderr, "posting input is unavailable\n");
            return 1;
        }
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        NSScreen *screen = NSScreen.mainScreen;
        if (!screen) {
            fprintf(stderr, "no main display is available\n");
            return 1;
        }
        NSRect visible = screen.visibleFrame;
        NSRect content = NSMakeRect(NSMinX(visible) + 35, NSMinY(visible) + 35, 260, 180);
        fixtureWindow = [[NSWindow alloc] initWithContentRect:content styleMask:NSWindowStyleMaskBorderless
            backing:NSBackingStoreBuffered defer:NO];
        fixtureWindow.releasedWhenClosed = NO;
        fixtureWindow.title = @"Jiffshot External Drop Fixture";
        fixtureWindow.level = NSStatusWindowLevel + 1;
        DropView *view = [[DropView alloc] initWithFrame:NSMakeRect(0, 0, 260, 180)];
        [view registerForDraggedTypes:@[NSPasteboardTypeFileURL]];
        fixtureWindow.contentView = view;
        [fixtureWindow orderFrontRegardless];
        [fixtureWindow display];
        NSPoint point = [fixtureWindow convertPointToScreen:NSMakePoint(130, 90)];
        targetPointer = CGPointMake(point.x, CGDisplayBounds(CGMainDisplayID()).size.height - point.y);
        if (!pointerPosition(&originalPointer)) {
            fprintf(stderr, "could not inspect the original pointer position\n");
            return 1;
        }
        if (!writePointerState(NO)) return 1;
        sourceMonitor = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, dispatch_get_main_queue());
        if (!sourceMonitor) {
            fprintf(stderr, "could not monitor the fixture source\n");
            return 1;
        }
        dispatch_source_set_timer(sourceMonitor, DISPATCH_TIME_NOW, 50 * NSEC_PER_MSEC, 10 * NSEC_PER_MSEC);
        dispatch_source_set_event_handler(sourceMonitor, ^{
            if (!finished && !sourceIsAlive()) finish(1, "the fixture source exited");
        });
        dispatch_resume(sourceMonitor);
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC / 4), dispatch_get_main_queue(), ^{
            if (finished) return;
            if (!sourceIsAlive()) {
                finish(1, "the fixture source exited before moving the pointer");
                return;
            }
            CGPoint current;
            if (!fixtureOwnsTarget() || !pointerPosition(&current) || !samePoint(current, originalPointer)
                    || otherInputActive()
                    || !CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft)) {
                fprintf(stderr, "target owner %ld, fixture %ld, target %.1f,%.1f, visible %d\n",
                    (long)targetOwner(), (long)fixtureWindow.windowNumber,
                    targetPointer.x, targetPointer.y, fixtureWindow.isVisible);
                finish(1, "the drop fixture is no longer safe to target");
                return;
            }
            CGError error = CGWarpMouseCursorPosition(targetPointer);
            if (error != kCGErrorSuccess) {
                finish(1, "could not move the pointer to the drop fixture");
                return;
            }
            movedPointer = YES;
            if (!writePointerState(YES)) {
                finish(1, "could not record the moved fixture pointer");
                return;
            }
            if (NSProcessInfo.processInfo.environment[@"JIFFSHOT_UITEST_DRAG_RECEIVER_HOLD_AFTER_WARP"]) {
                return;
            }
            if (NSProcessInfo.processInfo.environment[@"JIFFSHOT_UITEST_DRAG_RECEIVER_EXIT_AFTER_WARP"]) {
                if (NSProcessInfo.processInfo.environment[@"JIFFSHOT_UITEST_DRAG_RECEIVER_MOVE_BEFORE_EXIT"]) {
                    CGPoint current;
                    CGPoint changed = CGPointMake(targetPointer.x + 20, targetPointer.y);
                    NSPoint screenPoint = NSMakePoint(changed.x, CGDisplayBounds(CGMainDisplayID()).size.height - changed.y);
                    if (!pointerPosition(&current) || !samePoint(current, targetPointer) || otherInputActive() ||
                            !CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, kCGMouseButtonLeft) ||
                            [NSWindow windowNumberAtPoint:screenPoint belowWindowWithWindowNumber:0] != fixtureWindow.windowNumber ||
                            CGWarpMouseCursorPosition(changed) != kCGErrorSuccess) {
                        finish(1, "could not simulate fixture pointer movement");
                        return;
                    }
                }
                fprintf(stderr, "injected receiver exit after moving the fixture pointer\n");
                _exit(70);
            }
            if (!postMouse(kCGEventLeftMouseDragged)) {
                finish(1, "could not send the fixture drag event");
                return;
            }
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC / 4), dispatch_get_main_queue(), ^{
                if (finished) return;
                if (!sourceIsAlive()) {
                    finish(1, "the fixture source exited before the drop");
                    return;
                }
                CGPoint current;
                if (!pointerPosition(&current) || !samePoint(current, targetPointer)) {
                    finish(1, "the pointer changed before the fixture drop");
                } else if (!postMouse(kCGEventLeftMouseUp)) {
                    finish(1, "could not send the fixture drop event");
                } else {
                    releasedButton = YES;
                }
            });
        });
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 6 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
            finish(1, "the external AppKit receiver did not complete the drop");
        });
        [NSApp run];
        dispatch_source_cancel(sourceMonitor);
        [fixtureWindow close];
        return result;
    }
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (signal(SIGPIPE, SIG_IGN) == SIG_ERR) {
            fprintf(stderr, "could not protect the fixture from a closed output pipe\n");
            return 1;
        }
        if (argc == 4 && strcmp(argv[1], "--recover-synthetic-button") == 0) {
            return recoverSyntheticButton(argv[2], argv[3]);
        }
        const char *source = getenv("JIFFSHOT_UITEST_DRAG_SOURCE_PID");
        char *end = NULL;
        long pid = source ? strtol(source, &end, 10) : 0;
        if (!source || end == source || *end != '\0' || pid <= 1 || pid > INT_MAX) {
            fprintf(stderr, "the fixture source process is invalid\n");
            return recordReceiverExit() ? 2 : 1;
        }
        sourcePID = (pid_t)pid;
        if (!sourceIsAlive()) {
            fprintf(stderr, "the fixture source is no longer the receiver's parent\n");
            recordReceiverExit();
            return 1;
        }
        int status = runDropReceiver(argc, argv);
        return recordReceiverExit() ? status : 1;
    }
}
