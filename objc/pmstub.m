#import "IOPMLibPrivate.h"
#import <Foundation/Foundation.h>
#import <IOKit/pwr_mgt/IOPMLib.h>

bool getSleepDisabled() {
  CFDictionaryRef settings = IOPMCopySystemPowerSettings();
  CFBooleanRef sleepDisabled =
      CFDictionaryGetValue(settings, kIOPMSleepDisabledKey);
  return sleepDisabled == kCFBooleanTrue;
}
