#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>

static BOOL publish(NSWindow *window, NSWindow *duplicate, NSString *path) {
    NSMutableDictionary *info = [@{@"pid": @(NSProcessInfo.processInfo.processIdentifier),
                                    @"window": @(window.windowNumber)} mutableCopy];
    if (duplicate) {
        info[@"duplicate"] = @(duplicate.windowNumber);
    }
    NSError *error = nil;
    NSData *data = [NSJSONSerialization dataWithJSONObject:info options:0 error:&error];
    if (!data || ![data writeToFile:path options:NSDataWritingAtomic error:&error]) {
        fprintf(stderr, "could not publish fixture details: %s\n", error.description.UTF8String);
        return NO;
    }
    return YES;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc != 2 && !(argc == 3 && (strcmp(argv[2], "--minimized") == 0 ||
                                         strcmp(argv[2], "--hidden") == 0 ||
                                         strcmp(argv[2], "--ambiguous") == 0))) {
            fprintf(stderr, "expected a fixture information path and an optional window state\n");
            return 2;
        }
        BOOL minimized = argc == 3 && strcmp(argv[2], "--minimized") == 0;
        BOOL hidden = argc == 3 && strcmp(argv[2], "--hidden") == 0;
        BOOL ambiguous = argc == 3 && strcmp(argv[2], "--ambiguous") == 0;
        BOOL changeState = minimized || hidden || ambiguous;
        NSString *infoPath = [NSString stringWithUTF8String:argv[1]];
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        NSScreen *screen = [NSScreen mainScreen];
        if (!screen) {
            fprintf(stderr, "no main display\n");
            return 1;
        }
        NSRect visible = screen.visibleFrame;
        NSRect content = NSMakeRect(NSMinX(visible) + 32, NSMaxY(visible) - 200, 190, 130);
        NSWindow *window = [[NSWindow alloc] initWithContentRect:content
            styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskMiniaturizable backing:NSBackingStoreBuffered defer:NO];
        window.releasedWhenClosed = NO;
        window.title = @"Jiffshot External Control Fixture";
        window.opaque = YES;
        window.backgroundColor = NSColor.blueColor;
        if (!changeState) {
            NSRect frame = window.frame;
            NSWindow *backing = [[NSWindow alloc] initWithContentRect:NSInsetRect(frame, -12, -12)
                styleMask:NSWindowStyleMaskBorderless backing:NSBackingStoreBuffered defer:NO];
            backing.releasedWhenClosed = NO;
            backing.opaque = YES;
            backing.backgroundColor = NSColor.blueColor;
            [backing orderFrontRegardless];
        }
        NSWindow *duplicate = nil;
        if (ambiguous) {
            duplicate = [[NSWindow alloc] initWithContentRect:content
                styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskMiniaturizable backing:NSBackingStoreBuffered defer:NO];
            duplicate.releasedWhenClosed = NO;
            duplicate.title = window.title;
            duplicate.opaque = YES;
            duplicate.backgroundColor = NSColor.redColor;
            [duplicate orderFront:nil];
        }
        [app activateIgnoringOtherApps:YES];
        [window makeKeyAndOrderFront:nil];
        [window display];
        if (changeState) {
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 300 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
                if (hidden) {
                    [app hide:nil];
                } else {
                    [window miniaturize:nil];
                }
                dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 1000 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
                    if ((hidden ? !app.hidden : !window.miniaturized) || !publish(window, duplicate, infoPath)) {
                        fprintf(stderr, "could not change the fixture window state\n");
                        exit(1);
                    }
                });
            });
        } else if (!publish(window, nil, infoPath)) {
            return 1;
        }
        [app run];
    }
    return 0;
}
