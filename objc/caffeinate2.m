#import "IOPMLibPrivate.h"
#import <Foundation/Foundation.h>
#import <IOKit/pwr_mgt/IOPMLib.h>

bool disableSleep() {
  return IOPMSetSystemPowerSetting(kIOPMSleepDisabledKey, kCFBooleanTrue) == kIOReturnSuccess;
}

bool enableSleep() {
  return IOPMSetSystemPowerSetting(kIOPMSleepDisabledKey, kCFBooleanFalse) == kIOReturnSuccess;
}
